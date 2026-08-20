use crate::bus::Facades;
use crate::facaderpc::{self, Conn};
use crate::frame;
use crate::params::{array, i64_or, object, opt_text, str_of, strings, text_or, u64_or, value};
use crate::peer::Peer;
use crate::rpc::{Cmd, NodeView};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, watch};

type Answer = Result<Value, String>;
type Cmds = mpsc::UnboundedSender<Cmd>;

/// Mounting a plugin spawns a process and waits for its handshake, which the
/// kernel gives 30 s; a node request deadline of 10 s would report a timeout
/// for a mount that is merely slow.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Opts {
    pub root_env: String,
    pub timeout: Duration,
    pub facades: Option<Facades>,
}

pub async fn serve(listener: UnixListener, cmds: Cmds, opts: Opts) {
    let mut next = 1u64;
    while let Ok((stream, _address)) = listener.accept().await {
        let id = next;
        next += 1;
        tokio::spawn(connection(stream, id, cmds.clone(), opts.clone()));
    }
}

async fn connection(stream: UnixStream, id: u64, cmds: Cmds, opts: Opts) {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let peer = Peer::new(id, tx);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let conn = Conn::new(peer.clone(), opts.root_env.clone(), cancel_rx);
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if frame::write(&mut writer, &frame).await.is_err() {
                break;
            }
        }
    });
    while let Ok(Some(body)) = frame::read(&mut reader).await {
        if frame::method(&body) == Some("rep") {
            peer.resolve(&body);
            continue;
        }
        let (conn, cmds, opts) = (conn.clone(), cmds.clone(), opts.clone());
        tokio::spawn(async move {
            let outcome = dispatch(&body, &conn, &cmds, &opts).await;
            if let Some(id) = frame::id(&body) {
                conn.peer.send(frame::rep_id(id, outcome));
            }
        });
    }
    let _ = cancel_tx.send(true);
    peer.fail_all("disconnected");
    let _ = cmds.send(Cmd::Gone { peer: id });
}

async fn dispatch(body: &Value, conn: &Conn, cmds: &Cmds, opts: &Opts) -> Answer {
    let peer = &conn.peer;
    let method = frame::method(body).unwrap_or_default();
    let env = text_or(body, "env", &opts.root_env);
    match method {
        "node.register" => register(body, peer, cmds).await,
        "health" | "tree" | "reload" => forward(method, &env, cmds, opts).await,
        "svc" => svc(body, &env, cmds, opts).await,
        "plugin" => plugin(body, &env, cmds, opts).await,
        "session.create" | "session.prompt" | "session.status" | "session.history"
        | "session.resume" => session(method, body, &env, cmds, opts).await,
        "events.append" => {
            let kind = text_or(body, "kind", "");
            let data = object(body, "data");
            ask(cmds, |reply| Cmd::EventsAppend {
                env,
                kind,
                data,
                reply,
            })
            .await
        }
        "query.text" | "query.scan" | "query.vector" => {
            let requested = opt_text(body, "env");
            let env = conn.scoped_env(requested.as_deref())?;
            let params = body.clone();
            let method = method.to_string();
            ask(cmds, |reply| Cmd::Query {
                env,
                method,
                params,
                reply,
            })
            .await
        }
        "episodes.append" | "tool_results.append" | "blobs.put" | "blobs.get" | "state.retain" => {
            let params = body.clone();
            let method = method.to_string();
            ask(cmds, |reply| Cmd::Records {
                env,
                method,
                params,
                reply,
            })
            .await
        }
        "config.get" => ask(cmds, |reply| Cmd::ConfigGet { env, reply }).await,
        "config.patch" => {
            let patch = object(body, "patch");
            let target = text_or(body, "target", "env");
            ask(cmds, |reply| Cmd::ConfigPatch {
                env,
                target,
                patch,
                approved: false,
                reply,
            })
            .await
        }
        "runtime.register" => {
            let params = body.clone();
            let token = text_or(body, "token", "");
            ask(cmds, |reply| Cmd::RuntimeRegister {
                env,
                params,
                token,
                reply: Some(reply),
            })
            .await
        }
        "approval.request" => {
            let reason = text_or(body, "reason", "unspecified");
            let kind = text_or(body, "kind", "agent");
            ask(cmds, |reply| Cmd::Approval {
                env,
                reason,
                kind,
                reply,
            })
            .await
        }
        "approval.list" => {
            let status = str_of(body, "status")
                .filter(|status| *status != "all")
                .map(str::to_string);
            let limit = i64_or(body, "limit", 200);
            ask(cmds, |reply| Cmd::ApprovalList {
                status,
                limit,
                reply,
            })
            .await
        }
        "approval.answer" => {
            // `id` is the frame's own correlation id on every hop, so the
            // approval's id travels as `approval_id`, the way `plugin_id` does.
            let id = i64_or(body, "approval_id", 0);
            let decision = text_or(body, "decision", "approve");
            let note = opt_text(body, "note");
            ask(cmds, |reply| Cmd::ApprovalAnswer {
                id,
                decision,
                note,
                reply,
            })
            .await
        }
        "kill" | "resume" => {
            let on = method == "kill";
            let reason = text_or(body, "reason", "requested over the front door");
            ask(cmds, |reply| Cmd::Kill {
                on,
                reason,
                reply: Some(reply),
            })
            .await
        }
        "reset" => {
            let probes = strings(body, "probes");
            ask(cmds, |reply| Cmd::Reset { env, probes, reply }).await
        }
        "runtime.spawn" => {
            let parent = opt_text(body, "parent");
            let overrides = object(body, "overrides");
            let id = peer.id();
            ask(cmds, |reply| Cmd::Spawn {
                peer: id,
                parent,
                overrides,
                approved: false,
                reply,
            })
            .await
        }
        "runtime.stop" => ask(cmds, |reply| Cmd::RuntimeStop { env, reply }).await,
        "upgrade.propose" => {
            let params = body.clone();
            ask(cmds, |reply| Cmd::UpgradePropose { env, params, reply }).await
        }
        "upgrade.status" => {
            let id = body.get("upgrade_id").or_else(|| body.get("id_of"));
            let id = id.and_then(Value::as_i64).unwrap_or(0);
            ask(cmds, |reply| Cmd::UpgradeStatus { id, reply }).await
        }
        "upgrade.list" => {
            let env = opt_text(body, "env");
            let limit = i64_or(body, "limit", 50);
            ask(cmds, |reply| Cmd::UpgradeList { env, limit, reply }).await
        }
        "snap.list" => ask(cmds, |reply| Cmd::SnapList { env, reply }).await,
        "snap.export" => {
            let path = text_or(body, "path", "");
            ask(cmds, |reply| Cmd::SnapExport {
                env,
                path,
                approved: false,
                reply,
            })
            .await
        }
        "snap.pull" => {
            ask(cmds, |reply| Cmd::SnapPull {
                env,
                reply: Some(reply),
            })
            .await
        }
        "sandbox.exec" => sandbox_exec(body, env, cmds).await,
        "sandbox.destroy" => ask(cmds, |reply| Cmd::SandboxDestroy { env, reply }).await,
        "stop" => ask(cmds, |reply| Cmd::Stop { reply }).await,
        "status" => status(cmds, opts).await,
        "auth.scope" => facaderpc::scope(conn, body, cmds).await,
        #[cfg(feature = "http")]
        "ingress.register" => {
            let peer = peer.id();
            let name = text_or(body, "name", "");
            let port = i64_or(body, "port", 0);
            let public = body.get("public").and_then(Value::as_bool).unwrap_or(false);
            ask(cmds, |reply| Cmd::IngressRegister {
                peer,
                name,
                port,
                public,
                approved: false,
                reply,
            })
            .await
        }
        #[cfg(feature = "http")]
        "ingress.unregister" => {
            let peer = peer.id();
            let name = text_or(body, "name", "");
            ask(cmds, |reply| Cmd::IngressUnregister { peer, name, reply }).await
        }
        #[cfg(feature = "http")]
        "ingress.list" => {
            let facades = opts
                .facades
                .as_ref()
                .ok_or_else(|| "facades_unavailable".to_string())?;
            crate::ingress::list(facades, conn.bound_scope(), opt_text(body, "env"))
        }
        #[cfg(feature = "http")]
        "ingress.resolve" => {
            let facades = opts
                .facades
                .as_ref()
                .ok_or_else(|| "facades_unavailable".to_string())?;
            crate::ingress::resolve(facades, conn, &text_or(body, "name", ""))
        }
        "log.query" => {
            let after = i64_or(body, "after", 0);
            let limit = i64_or(body, "limit", 500);
            let session = opt_text(body, "session");
            ask(cmds, |reply| Cmd::LogQuery {
                env,
                after,
                limit,
                session,
                reply,
            })
            .await
        }
        #[cfg(feature = "http")]
        method if crate::secret::is_secret(method) => secret(method, body, conn, opts).await,
        method if is_facade(method) => facade(method, body, conn, cmds, opts).await,
        other => Err(format!("unknown_method:{other}")),
    }
}

/// The secrets facade (RFC 8d.4), reachable only in the `http` build. Values
/// live in base's own file; `get` is grant-checked against the caller's env.
#[cfg(feature = "http")]
async fn secret(method: &str, body: &Value, conn: &Conn, opts: &Opts) -> Answer {
    let Some(facades) = opts.facades.as_ref() else {
        return Err("facades_unavailable".to_string());
    };
    crate::secret::handle(method, body, conn, facades).await
}

fn is_facade(method: &str) -> bool {
    ["bus.", "kv.", "blob.", "timer."]
        .iter()
        .any(|prefix| method.starts_with(prefix))
}

/// Routes the four facade families to their handlers, which need the shared
/// hub/kv/blob/timer and the per-connection scope, not the actor. A streaming
/// subscribe marks the connection attached (RFC 8 UI-on-subscribe): its
/// departure is what `exit_on_detach` waits for.
async fn facade(method: &str, body: &Value, conn: &Conn, cmds: &Cmds, opts: &Opts) -> Answer {
    let Some(facades) = opts.facades.as_ref() else {
        return Err("facades_unavailable".to_string());
    };
    match method {
        "bus.publish" => facaderpc::bus_publish(conn, facades, body).await,
        "bus.subscribe" => {
            let _ = cmds.send(Cmd::Attach {
                peer: conn.peer.id(),
            });
            facaderpc::bus_subscribe(conn, facades, body)
        }
        method if method.starts_with("kv.") => facaderpc::kv(conn, facades, method, body),
        method if method.starts_with("blob.") => facaderpc::blob(conn, facades, method, body),
        method if method.starts_with("timer.") => facaderpc::timer(conn, facades, method, body),
        other => Err(format!("unknown_method:{other}")),
    }
}

async fn svc(body: &Value, env: &str, cmds: &Cmds, opts: &Opts) -> Answer {
    let node = peer_of(env, cmds).await?;
    let params = json!({
        "name": value(body, "name"),
        "method": value(body, "method"),
        "args": array(body, "args"),
    });
    node.request("svc", params, opts.timeout).await
}

/// Plugin management is the node's kernel, not base's: base only carries the
/// frame to that env's `Link`, which mounts, unmounts or restarts the fiber.
async fn plugin(body: &Value, env: &str, cmds: &Cmds, opts: &Opts) -> Answer {
    let node = peer_of(env, cmds).await?;
    let mut params = json!({"op": body.get("op").cloned().unwrap_or_else(|| json!("list"))});
    // The frame's own `id` correlates the request, so the fiber's id travels
    // as `plugin_id`.
    if let Some(value) = body.get("plugin_id") {
        params["plugin_id"] = value.clone();
    }
    for key in ["spec", "name", "config"] {
        if let Some(value) = body.get(key) {
            params[key] = value.clone();
        }
    }
    node.request("plugin", params, opts.timeout.max(MOUNT_TIMEOUT))
        .await
}

/// The CLI drives the env's harness through base: one `svc` frame to that
/// env's node, addressed to the harness's `loop` service.
async fn session(method: &str, body: &Value, env: &str, cmds: &Cmds, opts: &Opts) -> Answer {
    if matches!(method, "session.prompt" | "session.create") {
        let env = env.to_string();
        ask(cmds, |reply| Cmd::Guard { env, reply }).await?;
    }
    let node = peer_of(env, cmds).await?;
    let mut args = body.clone();
    if let Some(object) = args.as_object_mut() {
        for key in ["t", "id", "env"] {
            object.remove(key);
        }
    }
    let params = json!({"name": "loop", "method": method, "args": [args]});
    node.request("svc", params, opts.timeout).await
}

async fn sandbox_exec(body: &Value, env: String, cmds: &Cmds) -> Answer {
    let cmd = text_or(body, "cmd", "");
    let args = strings(body, "args");
    let timeout_ms = u64_or(body, "timeout", 30_000);
    ask(cmds, |reply| Cmd::SandboxExec {
        env,
        cmd,
        args,
        timeout_ms,
        reply,
    })
    .await
}

async fn register(body: &Value, peer: &Peer, cmds: &Cmds) -> Answer {
    let role = text_or(body, "role", "agent");
    let env = text_or(body, "env", "root");
    let pid = i64_or(body, "pid", 0);
    let token = text_or(body, "token", "");
    ask(cmds, |reply| Cmd::Register {
        peer: peer.clone(),
        role,
        env,
        pid,
        token,
        reply,
    })
    .await
}

async fn forward(method: &str, env: &str, cmds: &Cmds, opts: &Opts) -> Answer {
    let node = peer_of(env, cmds).await?;
    node.request(method, json!({}), opts.timeout).await
}

async fn status(cmds: &Cmds, opts: &Opts) -> Answer {
    let (tx, rx) = oneshot::channel();
    cmds.send(Cmd::Snapshot { reply: tx })
        .map_err(|_| "base_gone".to_string())?;
    let snapshot = rx.await.map_err(|_| "base_gone".to_string())?;
    let mut nodes = Vec::new();
    for node in &snapshot.nodes {
        nodes.push(node_json(node, opts).await);
    }
    Ok(json!({
        "home": snapshot.home,
        "release": snapshot.release,
        "killed": snapshot.killed,
        "pid": snapshot.pid,
        "exit_on_detach": snapshot.exit_on_detach,
        "attached": snapshot.attached,
        "nodes": nodes,
    }))
}

async fn node_json(node: &NodeView, opts: &Opts) -> Value {
    let tree = match &node.peer {
        Some(peer) => match peer.request("tree", json!({}), opts.timeout).await {
            Ok(result) => result.get("tree").cloned().unwrap_or(Value::Null),
            Err(error) => json!({ "error": error }),
        },
        None => Value::Null,
    };
    json!({
        "env": node.env,
        "role": node.role,
        "pid": node.pid,
        "registered": node.registered,
        "restarts": node.restarts,
        "sandbox": node.sandbox,
        "parent": node.parent,
        "depth": node.depth,
        "children": node.children,
        "worker": node.worker,
        "harness": node.harness,
        "runtime": node.runtime,
        "budget": node.budget,
        "tree": tree,
    })
}

async fn peer_of(env: &str, cmds: &Cmds) -> Result<Peer, String> {
    let (tx, rx) = oneshot::channel();
    cmds.send(Cmd::PeerOf {
        env: env.to_string(),
        reply: tx,
    })
    .map_err(|_| "base_gone".to_string())?;
    rx.await
        .map_err(|_| "base_gone".to_string())?
        .ok_or_else(|| format!("env {env} is not registered"))
}

async fn ask<F>(cmds: &Cmds, build: F) -> Answer
where
    F: FnOnce(oneshot::Sender<Answer>) -> Cmd,
{
    let (tx, rx) = oneshot::channel();
    cmds.send(build(tx)).map_err(|_| "base_gone".to_string())?;
    rx.await.map_err(|_| "base_gone".to_string())?
}
