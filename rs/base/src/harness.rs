use crate::base::Base;
use crate::home::Home;
use crate::node::{self, GUARDIAN};
use crate::peer::Peer;
use crate::rpc::Cmd;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const ROLE: &str = "harness";
pub const SERVICE: &str = "loop";
const POLL: Duration = Duration::from_millis(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Off,
    Booting,
    Ready(Option<i32>),
    Failed(String),
}

pub fn view(state: &State, restarts: u32) -> Value {
    match state {
        State::Off => json!({"state": "off"}),
        State::Booting => json!({"state": "booting"}),
        State::Ready(pid) => json!({"state": "ready", "pid": pid, "restarts": restarts}),
        State::Failed(reason) => json!({"state": "failed", "error": reason}),
    }
}

pub struct Running {
    pub pid: i32,
    pub exited: Option<oneshot::Receiver<Option<i32>>>,
}

/// One harness process per env, on the host, holding the model key: base
/// spawns it once that env's worker has answered, hands it the env's profile
/// overlay as JSON and the gateway address to register itself on, and never
/// lets it see anything of another env.
pub fn spawn(
    home: &Home,
    env: &str,
    config: &Value,
    generation: u64,
    exits: mpsc::UnboundedSender<Cmd>,
) -> Result<Running> {
    let exe = std::env::current_exe().context("locate the tenon binary")?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.log(&format!("harness-{env}")))
        .with_context(|| format!("open the harness log for {env}"))?;
    let mut command = tokio::process::Command::new(exe);
    command
        .arg(ROLE)
        .arg("--env")
        .arg(env)
        .current_dir(home.run())
        .env("TENON_ROLE", ROLE)
        .env("TENON_ENV", env)
        .env("TENON_HOME", &home.root)
        .env("TENON_BASE_SOCK", home.sock())
        .env("TENON_GATEWAY", home.gateway_address(env))
        .env("TENON_HARNESS_CONFIG", config.to_string());
    if let Some(name) = config
        .get("llm")
        .and_then(|llm| llm.get("api_key_env"))
        .and_then(Value::as_str)
    {
        if let Ok(key) = std::env::var(name) {
            command.env(name, key);
        }
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(false);
    let mut child = command.spawn().context("start the harness")?;
    let pid = child.id().context("harness has no pid")? as i32;
    let (tx, rx) = oneshot::channel();
    let env = env.to_string();
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|status| status.code());
        let _ = tx.send(code);
        let _ = exits.send(Cmd::HarnessExit {
            env,
            generation,
            code,
        });
    });
    Ok(Running {
        pid,
        exited: Some(rx),
    })
}

/// Polls the harness through the node's kernel until its `loop` service
/// answers, off the actor's task: the round trip goes base -> node -> gateway.
pub fn probe(
    env: String,
    peer: Peer,
    pid: i32,
    timeout: Duration,
    cmds: mpsc::UnboundedSender<Cmd>,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last = "no answer".to_string();
        while tokio::time::Instant::now() < deadline {
            let outcome = peer
                .request(
                    "svc",
                    json!({"name": SERVICE, "method": "sessions", "args": [{}]}),
                    PROBE_TIMEOUT,
                )
                .await;
            match outcome {
                Ok(_) => {
                    let _ = cmds.send(Cmd::HarnessReady {
                        env,
                        pid: Some(pid),
                        error: None,
                    });
                    return;
                }
                Err(error) => last = error,
            }
            tokio::time::sleep(POLL).await;
        }
        let _ = cmds.send(Cmd::HarnessReady {
            env,
            pid: None,
            error: Some(last),
        });
    });
}

impl Base {
    pub fn harness_boot(&mut self, env: &str) {
        if env == GUARDIAN || self.halted(env) {
            return;
        }
        let config = match self.harness_settings(env) {
            Ok(config) => config,
            Err(error) => {
                self.emit(
                    "harness.failed",
                    Some(env),
                    json!({"error": error.to_string()}),
                );
                return;
            }
        };
        let generation = self.nodes.get(env).map(|node| node.generation).unwrap_or(0);
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        if matches!(node.harness, State::Booting | State::Ready(_)) {
            return;
        }
        node.harness = State::Booting;
        let peer = node.peer.clone();
        let running = match spawn(&self.home, env, &config, generation, self.cmds.clone()) {
            Ok(running) => running,
            Err(error) => {
                self.harness_ready(env, None, Some(error.to_string()));
                return;
            }
        };
        let pid = running.pid;
        if let Some(node) = self.nodes.get_mut(env) {
            node.harness_pid = Some(pid);
            node.harness_exited = running.exited;
        }
        self.emit("harness.boot", Some(env), json!({"pid": pid}));
        if let Some(peer) = peer {
            let timeout = Duration::from_millis(self.config.worker.boot_timeout_ms);
            probe(env.to_string(), peer, pid, timeout, self.cmds.clone());
        }
    }

    /// The env's overlay as the harness receives it, with base's own
    /// `gated_tools` seeded in when the profile names none of its own: the
    /// list is a host rule, not an agent preference.
    pub fn harness_settings(&self, env: &str) -> anyhow::Result<Value> {
        let mut config = self.home.harness_config(env)?;
        let gated = &self.config.approval.gated_tools;
        if gated.is_empty() || config.get("gated_tools").is_some() {
            return Ok(config);
        }
        if let Some(object) = config.as_object_mut() {
            object.insert("gated_tools".to_string(), json!(gated));
        }
        Ok(config)
    }

    pub fn harness_ready(&mut self, env: &str, pid: Option<i32>, error: Option<String>) {
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        match error {
            Some(reason) => {
                node.harness = State::Failed(reason.clone());
                self.emit("harness.failed", Some(env), json!({"error": reason}));
            }
            None => {
                node.harness = State::Ready(pid);
                self.emit("harness.ready", Some(env), json!({"pid": pid}));
            }
        }
    }

    /// A harness that dies is restarted while the env stays up: its sessions
    /// live in the event log, so a fresh one resumes them on request.
    pub fn harness_exit(&mut self, env: &str, generation: u64, code: Option<i32>) {
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        if self.stopping || node.generation != generation || node.harness_pid.is_none() {
            return;
        }
        node.harness = State::Off;
        node.harness_pid = None;
        node.harness_exited = None;
        node.harness_restarts += 1;
        let restarts = node.harness_restarts;
        self.emit(
            "harness.exit",
            Some(env),
            json!({"code": code, "restarts": restarts}),
        );
        if restarts <= self.config.max_restarts && !self.halted(env) {
            self.harness_boot(env);
        }
    }

    pub async fn harness_halt(&mut self, env: &str, grace: Duration) {
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        node.harness = State::Off;
        let pid = node.harness_pid.take();
        let exited = node.harness_exited.take();
        if let Some(pid) = pid {
            node::terminate(pid, exited, grace).await;
        }
    }
}
