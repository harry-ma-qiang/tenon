use crate::frame;
use crate::params::{array, i64_or, object, opt_text, str_of, strings, text_or, u64_or, value};
use crate::peer::Peer;
use crate::rpc::{Cmd, NodeView};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

type Answer = Result<Value, String>;
type Cmds = mpsc::UnboundedSender<Cmd>;

/// Mounting a plugin spawns a process and waits for its handshake, which the
/// kernel gives 30 s; a node request deadline of 10 s would report a timeout
/// for a mount that is merely slow.
const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Opts {
    pub root_env: String,
    pub timeout: Duration,
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
        let (peer, cmds, opts) = (peer.clone(), cmds.clone(), opts.clone());
        tokio::spawn(async move {
            let outcome = dispatch(&body, &peer, &cmds, &opts).await;
            if let Some(id) = frame::id(&body) {
                peer.send(reply(id, outcome));
            }
        });
    }
    peer.fail_all("disconnected");
    let _ = cmds.send(Cmd::Gone { peer: id });
}

fn reply(id: u64, outcome: Answer) -> Value {
    match outcome {
        Ok(result) => json!({"t": "rep", "id": id, "result": result}),
        Err(error) => json!({"t": "rep", "id": id, "error": error}),
    }
}

async fn dispatch(body: &Value, peer: &Peer, cmds: &Cmds, opts: &Opts) -> Answer {
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
        "events.tail" => {
            let after = i64_or(body, "after", 0);
            let limit = i64_or(body, "limit", 500);
            ask(cmds, |reply| Cmd::EventsTail {
                env,
                after,
                limit,
                reply,
            })
            .await
        }
        "episodes.append"
        | "episodes.tail"
        | "tool_results.append"
        | "tool_results.tail"
        | "blobs.put"
        | "blobs.get"
        | "state.retain" => {
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
        "subscribe" => subscribe(peer, body, cmds).await,
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

async fn subscribe(peer: &Peer, body: &Value, cmds: &Cmds) -> Answer {
    let (tx, rx) = oneshot::channel();
    cmds.send(Cmd::Subscribe {
        peer: peer.clone(),
        env: opt_text(body, "env"),
        reply: tx,
    })
    .map_err(|_| "base_gone".to_string())?;
    rx.await.map_err(|_| "base_gone".to_string())
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
