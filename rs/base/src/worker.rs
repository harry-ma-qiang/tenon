use crate::peer::Peer;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::Instance;
use tokio::sync::mpsc;

pub const SERVICE: &str = "worker";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(300);
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const LOG: &str = ".tenon-worker.log";

pub struct Pulled {
    pub step: i64,
    pub reference: String,
    pub bytes: Vec<u8>,
}

/// Waits until the env's gateway is actually accepting connections. A unix
/// gateway is a file that appears; a tcp gateway is a port that starts
/// answering, and there is no file to watch for it.
async fn listening(gateway: &str, deadline: tokio::time::Instant) -> Result<(), String> {
    loop {
        if let Some(path) = gateway.strip_prefix("unix:") {
            if Path::new(path).exists() {
                return Ok(());
            }
        } else if let Some(address) = gateway.strip_prefix("tcp:") {
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return Ok(());
            }
        } else {
            return Err(format!("bad gateway address {gateway}"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("gateway {gateway} never came up"));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Starts the resident worker inside an env's sandbox instance and reports
/// back once it answers on the wire. Every step runs off the actor's task: the
/// container exec is a blocking call and the readiness poll is a round trip
/// through the node's gateway.
pub fn boot(
    env: String,
    instance: Arc<dyn Instance>,
    peer: Peer,
    gateway: String,
    timeout: Duration,
    cmds: mpsc::UnboundedSender<Cmd>,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + timeout;
        if let Some(error) = launch(&env, instance, &gateway, deadline).await.err() {
            let _ = cmds.send(Cmd::WorkerReady {
                env,
                pid: None,
                error: Some(error),
            });
            return;
        }
        let outcome = ready(&peer, deadline).await;
        let _ = cmds.send(match outcome {
            Ok(pid) => Cmd::WorkerReady {
                env,
                pid,
                error: None,
            },
            Err(error) => Cmd::WorkerReady {
                env,
                pid: None,
                error: Some(error),
            },
        });
    });
}

async fn launch(
    env: &str,
    instance: Arc<dyn Instance>,
    gateway: &str,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    listening(gateway, deadline).await?;
    // A VM backend boots the worker as the guest init, which is why the wait
    // above matters for it too: there is no second chance to connect, and the
    // guest starts the moment the instance is told to.
    let address = gateway.to_string();
    let owned = instance.clone();
    let name = env.to_string();
    let took = tokio::task::spawn_blocking(move || owned.start_worker(&name, &address))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if took {
        return Ok(());
    }
    let workspace = instance.workspace_path();
    let binary = instance.binary_path();
    let address = gateway;
    let line = format!(
        "cd {workspace} && TENON_GATEWAY={address} TENON_ENV={env} nohup {binary} worker \
         --workspace {workspace} >> {workspace}/{LOG} 2>&1 </dev/null & echo started"
    );
    let outcome = tokio::task::spawn_blocking(move || {
        instance.exec("sh", &["-c".to_string(), line], LAUNCH_TIMEOUT)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    if outcome.status != 0 {
        return Err(format!(
            "worker launch exited {}: {}",
            outcome.status,
            String::from_utf8_lossy(&outcome.stderr).trim()
        ));
    }
    Ok(())
}

async fn ready(peer: &Peer, deadline: tokio::time::Instant) -> Result<Option<i64>, String> {
    let mut last = "no answer".to_string();
    while tokio::time::Instant::now() < deadline {
        match svc(peer, "info", json!({})).await {
            Ok(info) => return Ok(info.get("pid").and_then(Value::as_i64)),
            Err(error) => last = error,
        }
        tokio::time::sleep(POLL).await;
    }
    Err(last)
}

pub async fn svc(peer: &Peer, method: &str, params: Value) -> Result<Value, String> {
    peer.request(
        "svc",
        json!({"name": SERVICE, "method": method, "args": [params]}),
        REQUEST_TIMEOUT,
    )
    .await
}

/// Asks the worker for everything it has committed since the last pack this
/// env's state file acknowledged. The acknowledgement is exactly that stored
/// step: the next pull asks for `since` and the worker never resends it.
pub async fn pull(
    peer: &Peer,
    since: i64,
    workspace: &Path,
    guest_workspace: &str,
) -> Result<Option<Pulled>, String> {
    let answer = svc(peer, "snap.pack", json!({ "since": since })).await?;
    let step = answer.get("step").and_then(Value::as_i64).unwrap_or(0);
    let reference = answer
        .get("ref")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if step <= since || reference.is_empty() {
        return Ok(None);
    }
    let bytes = match answer.get("handle").and_then(Value::as_str) {
        Some(handle) => {
            let path = host_path(handle, workspace, guest_workspace);
            std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?
        }
        None => {
            let body = answer
                .get("pack")
                .and_then(Value::as_str)
                .ok_or_else(|| "snap.pack answered without pack or handle".to_string())?;
            decode(body)?
        }
    };
    Ok(Some(Pulled {
        step,
        reference,
        bytes,
    }))
}

/// The restore path: base has already written every stored pack into the fresh
/// workspace, so the worker only has to fold them into a new `.tenon-snap` and
/// check the newest ref out.
pub async fn apply(
    peer: &Peer,
    rows: &[(i64, String)],
    guest_workspace: &str,
    head: &str,
) -> Result<Value, String> {
    let packs: Vec<Value> = rows
        .iter()
        .map(|(step, reference)| {
            json!({
                "step": step,
                "ref": reference,
                "handle": format!("{guest_workspace}/.tenon-restore/{step}.pack"),
            })
        })
        .collect();
    svc(peer, "snap.apply", json!({"packs": packs, "ref": head})).await
}

pub fn host_path(handle: &str, workspace: &Path, guest_workspace: &str) -> PathBuf {
    match handle.strip_prefix(guest_workspace) {
        Some(rest) => workspace.join(rest.trim_start_matches('/')),
        None => PathBuf::from(handle),
    }
}

fn decode(body: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|error| format!("bad base64 pack: {error}"))
}
