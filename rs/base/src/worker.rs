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
#[allow(clippy::too_many_arguments)]
pub fn boot(
    env: String,
    instance: Arc<dyn Instance>,
    peer: Peer,
    gateway: String,
    spec: Option<Value>,
    timeout: Duration,
    cmds: mpsc::UnboundedSender<Cmd>,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + timeout;
        if let Some(error) = launch(&env, instance, &gateway, spec, deadline).await.err() {
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
    spec: Option<Value>,
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
    // A promoted candidate worker replaces the built-in launch line; the
    // built-in one stays the LKG fallback and is what a rollback comes back to.
    // Its own variables go in front of `nohup`, which takes a command, not an
    // assignment.
    let (exports, command) = match &spec {
        Some(spec) => (candidate_env(spec), candidate_cmd(spec)),
        None => (
            String::new(),
            format!("{binary} worker --workspace {workspace}"),
        ),
    };
    let line = format!(
        "cd {workspace} && TENON_GATEWAY={address} TENON_ENV={env} \
         TENON_WORKSPACE={workspace} {exports}nohup {command} >> {workspace}/{LOG} 2>&1 \
         </dev/null & echo started"
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

/// The `env` of a candidate worker spec, as shell assignments.
pub fn candidate_env(spec: &Value) -> String {
    let mut line = String::new();
    for pair in spec["env"].as_array().cloned().unwrap_or_default() {
        let name = pair[0].as_str().unwrap_or_default();
        let value = pair[1].as_str().unwrap_or_default();
        if !name.is_empty() {
            line.push_str(&format!("{name}={value} "));
        }
    }
    line
}

/// The `cmd` and `args` of a candidate worker spec, as one command line: the
/// same shape `upgrade.propose{target: worker}` takes.
pub fn candidate_cmd(spec: &Value) -> String {
    let mut line = spec["cmd"].as_str().unwrap_or_default().to_string();
    for arg in spec["args"].as_array().cloned().unwrap_or_default() {
        if let Some(arg) = arg.as_str() {
            line.push(' ');
            line.push_str(arg);
        }
    }
    line
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
    let step = crate::params::i64_or(&answer, "step", 0);
    let reference = crate::params::text(&answer, "ref");
    if step <= since || reference.is_empty() {
        return Ok(None);
    }
    let bytes = match crate::params::str_of(&answer, "handle") {
        Some(handle) => {
            let path = host_path(handle, workspace, guest_workspace);
            std::fs::read(&path).map_err(|error| format!("read {}: {error}", path.display()))?
        }
        None => {
            let body = crate::params::str_of(&answer, "pack")
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
