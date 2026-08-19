use crate::base::Base;
use crate::peer::Peer;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

type Answer = Result<Value, String>;

pub const VERSION: &str = "1";
pub const DEFAULT_NAME: &str = "tenon-default";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_LIMIT: usize = 64 * 1024;

/// What a runtime told base about itself, once the contract check and the
/// health probe both passed. One row per environment: registering again
/// replaces it, which is how a replaced runtime announces the swap.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub env: String,
    pub name: String,
    pub version: String,
    pub hash: String,
    pub health_kind: String,
    pub health_target: String,
    pub events: String,
    pub approvals: String,
    pub at: i64,
    pub probe_ms: i64,
}

impl Runtime {
    pub fn view(&self) -> Value {
        json!({
            "contract": VERSION,
            "env": self.env,
            "manifest": {"name": self.name, "version": self.version, "hash": self.hash},
            "health": {"kind": self.health_kind, "target": self.health_target},
            "channels": {"events": self.events, "approvals": self.approvals},
            "at": self.at,
            "probe_ms": self.probe_ms,
        })
    }
}

fn field(object: &Value, key: &str, name: &str) -> Result<String, String> {
    let value = crate::params::text(object, key).trim().to_string();
    match value.is_empty() {
        true => Err(format!("runtime contract: {name}.{key} is required")),
        false => Ok(value),
    }
}

/// The contract of RFC section 2, as a function: a runtime that does not
/// carry a manifest, a health target base can reach and the two channel
/// names is refused before anything is probed or recorded.
pub fn contract(env: &str, params: &Value) -> Result<Runtime, String> {
    let manifest = params
        .get("manifest")
        .filter(|value| value.is_object())
        .ok_or_else(|| "runtime contract: manifest is required".to_string())?;
    let health = params
        .get("health")
        .filter(|value| value.is_object())
        .ok_or_else(|| "runtime contract: health is required".to_string())?;
    let channels = params
        .get("channels")
        .filter(|value| value.is_object())
        .ok_or_else(|| "runtime contract: channels is required".to_string())?;
    let kind = field(health, "kind", "health")?;
    if kind != "rpc" && kind != "http" {
        return Err(format!(
            "runtime contract: health.kind must be rpc or http, not {kind}"
        ));
    }
    let target = field(health, "target", "health")?;
    if kind == "rpc" && !target.contains('.') {
        return Err(format!(
            "runtime contract: health.target must be service.method, not {target}"
        ));
    }
    Ok(Runtime {
        env: env.to_string(),
        name: field(manifest, "name", "manifest")?,
        version: field(manifest, "version", "manifest")?,
        hash: field(manifest, "hash", "manifest")?,
        health_kind: kind,
        health_target: target,
        events: field(channels, "events", "channels")?,
        approvals: field(channels, "approvals", "channels")?,
        at: tenon_storage::now(),
        probe_ms: 0,
    })
}

/// Base's own half of the contract: it does not take the runtime's word, it
/// calls the health target the runtime declared. `rpc` goes through that
/// env's node as an ordinary `svc` frame, `http` is a plain GET.
pub async fn probe(runtime: &Runtime, peer: Option<Peer>) -> Result<i64, String> {
    let started = std::time::Instant::now();
    match runtime.health_kind.as_str() {
        "rpc" => {
            let peer = peer.ok_or_else(|| format!("env {} is not registered", runtime.env))?;
            let (name, method) = runtime
                .health_target
                .rsplit_once('.')
                .ok_or_else(|| "health.target must be service.method".to_string())?;
            peer.request(
                "svc",
                json!({"name": name, "method": method, "args": [{}]}),
                PROBE_TIMEOUT,
            )
            .await?;
        }
        _ => get(&runtime.health_target).await?,
    }
    Ok(started.elapsed().as_millis() as i64)
}

/// A GET against a loopback health endpoint, hand-rolled for the same reason
/// `serve --http` is: four lines of HTTP/1.0 do not pay for a client stack.
async fn get(target: &str) -> Result<(), String> {
    let rest = target
        .strip_prefix("http://")
        .ok_or_else(|| format!("health.target {target} is not an http:// url"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let address = match authority.contains(':') {
        true => authority.to_string(),
        false => format!("{authority}:80"),
    };
    let mut stream = tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(&address))
        .await
        .map_err(|_| format!("health {target}: connect timed out"))?
        .map_err(|error| format!("health {target}: {error}"))?;
    let request = format!("GET {path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| format!("health {target}: {error}"))?;
    let mut body = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = tokio::time::timeout(PROBE_TIMEOUT, stream.read(&mut buffer))
            .await
            .map_err(|_| format!("health {target}: read timed out"))?
            .map_err(|error| format!("health {target}: {error}"))?;
        if read == 0 || body.len() >= HTTP_LIMIT {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    let head = String::from_utf8_lossy(&body);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    match (200..400).contains(&status) {
        true => Ok(()),
        false => Err(format!("health {target}: status {status}")),
    }
}

/// The manifest hash of the shipped runtime: the binary that runs the
/// harness, the worker and node A. Read once per process, off the actor.
pub fn self_hash() -> String {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else {
            return "unknown".to_string();
        };
        let Ok(bytes) = std::fs::read(&exe) else {
            return "unknown".to_string();
        };
        let sum = Sha256::digest(&bytes);
        sum.iter().map(|byte| format!("{byte:02x}")).collect()
    })
    .clone()
}

pub fn default_manifest(hash: String) -> Value {
    json!({
        "manifest": {"name": DEFAULT_NAME, "version": env!("CARGO_PKG_VERSION"), "hash": hash},
        "health": {"kind": "rpc", "target": "loop.ping"},
        "channels": {"events": "events.append", "approvals": "approval.request"},
    })
}

impl Base {
    /// The per-env runtime token: generated with the env, handed to that env's
    /// harness in its environment and written to `run/rt-<env>.token` with
    /// owner-only permissions, so a runtime a human starts by hand (DSH
    /// through the bridge) has a legitimate way to authenticate.
    pub fn write_runtime_token(&self, env: &str, token: &str) {
        let path = self.home.runtime_token_file(env);
        if std::fs::write(&path, token).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }

    pub fn runtime_view(&self, env: &str) -> Option<Value> {
        self.runtimes.get(env).map(Runtime::view)
    }

    /// `runtime.register`: authenticate, check the contract, probe the health
    /// target the runtime declared, and only then record it. Every refusal
    /// carries the reason, both to the caller and into the log.
    pub fn runtime_register(
        &mut self,
        env: &str,
        params: &Value,
        token: &str,
        reply: Option<oneshot::Sender<Answer>>,
    ) {
        let outcome = self.check_runtime(env, params, token);
        let runtime = match outcome {
            Ok(runtime) => runtime,
            Err(reason) => {
                self.emit_env(env, "runtime.refused", json!({"reason": reason}));
                answer(reply, Err(reason));
                return;
            }
        };
        let peer = self.nodes.get(env).and_then(|node| node.peer.clone());
        let cmds = self.cmds.clone();
        tokio::spawn(async move {
            let outcome = probe(&runtime, peer).await;
            let _ = cmds.send(Cmd::RuntimeProbed {
                runtime: Box::new(runtime),
                outcome,
                reply,
            });
        });
    }

    fn check_runtime(&self, env: &str, params: &Value, token: &str) -> Result<Runtime, String> {
        let Some(node) = self.nodes.get(env) else {
            return Err(format!("unknown env {env}"));
        };
        if node.runtime_token != token {
            return Err("unauthorized".to_string());
        }
        contract(env, params)
    }

    pub fn runtime_probed(
        &mut self,
        mut runtime: Runtime,
        outcome: Result<i64, String>,
        reply: Option<oneshot::Sender<Answer>>,
    ) {
        let env = runtime.env.clone();
        match outcome {
            Err(error) => {
                let reason = format!("health probe failed: {error}");
                self.emit_env(
                    &env,
                    "runtime.refused",
                    json!({"reason": reason, "name": runtime.name}),
                );
                answer(reply, Err(reason));
            }
            Ok(probe_ms) => {
                runtime.probe_ms = probe_ms;
                let view = runtime.view();
                self.runtimes.insert(env.clone(), runtime);
                self.emit_env(&env, "runtime.register", view.clone());
                answer(reply, Ok(view));
            }
        }
    }

    /// Base registers the default runtime on behalf of its own env: node A,
    /// the worker and the harness are one runtime, and the harness answering
    /// `loop.ping` is what proves it alive.
    pub fn register_default_runtime(&mut self, env: &str) {
        let Some(node) = self.nodes.get(env) else {
            return;
        };
        let token = node.runtime_token.clone();
        let env = env.to_string();
        let cmds = self.cmds.clone();
        tokio::spawn(async move {
            let hash = tokio::task::spawn_blocking(self_hash)
                .await
                .unwrap_or_else(|_| "unknown".to_string());
            let _ = cmds.send(Cmd::RuntimeRegister {
                env,
                params: default_manifest(hash),
                token,
                reply: None,
            });
        });
    }
}

fn answer(reply: Option<oneshot::Sender<Answer>>, outcome: Answer) {
    if let Some(reply) = reply {
        let _ = reply.send(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> Value {
        json!({
            "manifest": {"name": "dsh", "version": "0.9.1", "hash": "abc123"},
            "health": {"kind": "rpc", "target": "dsh.ping"},
            "channels": {"events": "events.append", "approvals": "approval.request"},
        })
    }

    #[test]
    fn a_complete_manifest_passes_the_contract() {
        let runtime = contract("root", &good()).expect("contract");
        assert_eq!(runtime.name, "dsh");
        assert_eq!(runtime.health_target, "dsh.ping");
        assert_eq!(runtime.view()["contract"], VERSION);
    }

    #[test]
    fn every_missing_piece_is_refused_by_name() {
        for (key, needle) in [
            ("manifest", "manifest is required"),
            ("health", "health is required"),
            ("channels", "channels is required"),
        ] {
            let mut params = good();
            params.as_object_mut().expect("object").remove(key);
            let error = contract("root", &params).expect_err("refused");
            assert!(error.contains(needle), "{error}");
        }
        let mut params = good();
        params["manifest"]["version"] = json!("");
        assert!(contract("root", &params)
            .expect_err("refused")
            .contains("manifest.version"));
        let mut params = good();
        params["health"]["kind"] = json!("carrier-pigeon");
        assert!(contract("root", &params)
            .expect_err("refused")
            .contains("rpc or http"));
        let mut params = good();
        params["health"]["target"] = json!("ping");
        assert!(contract("root", &params)
            .expect_err("refused")
            .contains("service.method"));
    }
}
