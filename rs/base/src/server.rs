use crate::base::{Cmd, NodeView};
use crate::frame;
use crate::peer::Peer;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

type Answer = Result<Value, String>;
type Cmds = mpsc::UnboundedSender<Cmd>;

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
        "node.register" => register(body, peer, cmds),
        "health" | "tree" | "reload" => forward(method, &env, cmds, opts).await,
        "reset" => ask(cmds, |reply| Cmd::Reset { env, reply }).await,
        "stop" => ask(cmds, |reply| Cmd::Stop { reply }).await,
        "status" => status(cmds, opts).await,
        "subscribe" => subscribe(peer, body, cmds).await,
        other => Err(format!("unknown_method:{other}")),
    }
}

fn register(body: &Value, peer: &Peer, cmds: &Cmds) -> Answer {
    let role = string(body, "role", "agent");
    let env = string(body, "env", "root");
    let pid = body.get("pid").and_then(Value::as_i64).unwrap_or(0);
    cmds.send(Cmd::Register {
        peer: peer.clone(),
        role,
        env,
        pid,
    })
    .map_err(|_| "base_gone".to_string())?;
    Ok(json!({"ok": true}))
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
