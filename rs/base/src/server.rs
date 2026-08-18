use crate::frame;
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
    let env = body
        .get("env")
        .and_then(Value::as_str)
        .unwrap_or(&opts.root_env)
        .to_string();
    match method {
        "node.register" => register(body, peer, cmds).await,
        "health" | "tree" | "reload" => forward(method, &env, cmds, opts).await,
        "svc" => svc(body, &env, cmds, opts).await,
        "plugin" => plugin(body, &env, cmds, opts).await,
        "session.create" | "session.prompt" | "session.status" | "session.history"
        | "session.resume" => session(method, body, &env, cmds, opts).await,
        "events.append" => {
            let kind = string(body, "kind", "");
            let data = body.get("data").cloned().unwrap_or_else(|| json!({}));
            ask(cmds, |reply| Cmd::EventsAppend {
                env,
                kind,
                data,
                reply,
            })
            .await
        }
        "events.tail" => {
            let after = body.get("after").and_then(Value::as_i64).unwrap_or(0);
            let limit = body.get("limit").and_then(Value::as_i64).unwrap_or(500);
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
            let patch = body.get("patch").cloned().unwrap_or_else(|| json!({}));
            ask(cmds, |reply| Cmd::ConfigPatch { env, patch, reply }).await
        }
        "approval.request" => {
            let reason = string(body, "reason", "unspecified");
            ask(cmds, |reply| Cmd::Approval { env, reason, reply }).await
        }
        "reset" => ask(cmds, |reply| Cmd::Reset { env, reply }).await,
        "runtime.spawn" => {
            let parent = body
                .get("parent")
                .and_then(Value::as_str)
                .map(str::to_string);
            let overrides = body.get("overrides").cloned().unwrap_or_else(|| json!({}));
            let id = peer.id();
            ask(cmds, |reply| Cmd::Spawn {
                peer: id,
                parent,
                overrides,
                reply,
            })
            .await
        }
        "runtime.stop" => ask(cmds, |reply| Cmd::RuntimeStop { env, reply }).await,
        "snap.list" => ask(cmds, |reply| Cmd::SnapList { env, reply }).await,
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
        "name": body.get("name").cloned().unwrap_or(Value::Null),
        "method": body.get("method").cloned().unwrap_or(Value::Null),
        "args": body.get("args").cloned().unwrap_or_else(|| json!([])),
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
    let cmd = string(body, "cmd", "");
    let args = body
        .get("args")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let timeout_ms = body
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(30_000);
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
    let role = string(body, "role", "agent");
    let env = string(body, "env", "root");
    let pid = body.get("pid").and_then(Value::as_i64).unwrap_or(0);
    let token = string(body, "token", "");
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
        "tree": tree,
    })
}

async fn subscribe(peer: &Peer, body: &Value, cmds: &Cmds) -> Answer {
    let (tx, rx) = oneshot::channel();
    cmds.send(Cmd::Subscribe {
        peer: peer.clone(),
        env: body.get("env").and_then(Value::as_str).map(str::to_string),
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

fn string(body: &Value, key: &str, fallback: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}
