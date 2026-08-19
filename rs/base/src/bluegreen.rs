use crate::base::Base;
use crate::drive::{Answer, Drive};
use crate::node::{self, GUARDIAN};
use crate::rpc::Cmd;
use crate::state::{Node, WorkerState};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

pub const SUFFIX: &str = "~green";
const REGISTER_TIMEOUT: Duration = Duration::from_secs(90);
const POLL: Duration = Duration::from_millis(300);

pub fn green_of(env: &str) -> String {
    format!("{env}{SUFFIX}")
}

/// The candidate release: a copy of the running one with the candidate
/// `tenon.beam` in place of the shipped module. A node boots from a directory,
/// and a release in embedded mode loads its modules from that directory's lib
/// tree, so replacing the file is the whole of "run the new kernel".
pub fn stage(home: &crate::home::Home, id: i64, release: &Path, beam: &str) -> Result<PathBuf> {
    let into = home.upgrade_dir(id).join("release");
    let _ = std::fs::remove_dir_all(&into);
    std::fs::create_dir_all(&into).with_context(|| format!("create {}", into.display()))?;
    let status = std::process::Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", release.display()))
        .arg(&into)
        .status()
        .context("copy the release")?;
    if !status.success() {
        bail!("copying {} exited {status}", release.display());
    }
    let target = crate::check::shipped_beam(&into)?;
    std::fs::copy(beam, &target)
        .with_context(|| format!("stage {beam} as {}", target.display()))?;
    Ok(into)
}

async fn peer_of(drive: &Drive, env: &str) -> Option<crate::peer::Peer> {
    let (tx, rx) = oneshot::channel();
    drive
        .cmds
        .send(Cmd::PeerOf {
            env: env.to_string(),
            reply: tx,
        })
        .ok()?;
    rx.await.ok().flatten()
}

/// The health gate in front of the switch: A' has to register with base and
/// answer with a healthy tree before the front door moves. A beam that never
/// gets that far leaves A untouched.
pub async fn await_green(drive: &Drive, green: &str) -> Result<(), String> {
    let deadline = Instant::now() + REGISTER_TIMEOUT;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        let status = drive
            .ask(|reply| Cmd::EnvStatus {
                env: green.to_string(),
                reply,
            })
            .await?;
        last = status.clone();
        if status["registered"] == json!(true) {
            return healthy(drive, green).await;
        }
        tokio::time::sleep(POLL).await;
    }
    Err(format!("the new kernel node never registered: {last}"))
}

async fn healthy(drive: &Drive, green: &str) -> Result<(), String> {
    let Some(peer) = peer_of(drive, green).await else {
        return Err("the new kernel node has no connection".to_string());
    };
    let health = peer.request("health", json!({}), drive.timeout).await?;
    if health["ok"] != json!(true) {
        return Err(format!("the new kernel node is not healthy: {health}"));
    }
    let tree = peer.request("tree", json!({}), drive.timeout).await?;
    match tree["tree"]["status"] == json!("active") {
        true => Ok(()),
        false => Err(format!("the new kernel node's tree is not active: {tree}")),
    }
}

impl Base {
    /// Blue: node A keeps serving. Green: a second agent node from the
    /// candidate release, with the same profile, on its own socket **inside
    /// the same gateway directory** — that directory is what the sandbox
    /// mounts, so the worker can reach either node without a new mount.
    pub fn kernel_switch(
        &mut self,
        id: i64,
        env: String,
        release: PathBuf,
        reply: oneshot::Sender<Answer>,
    ) {
        let green = green_of(&env);
        let Some(node) = self.nodes.get(&env) else {
            let _ = reply.send(Err(format!("unknown env {env}")));
            return;
        };
        if node.role == GUARDIAN {
            let _ = reply.send(Err("the guardian node is L0".to_string()));
            return;
        }
        let address = self.gateway_address(&env);
        if !address.starts_with("unix:") {
            let _ = reply.send(Err(format!(
                "blue/green needs a unix gateway, and {env} listens on {address}"
            )));
            return;
        }
        let profile = node.profile.clone();
        let depth = node.depth;
        let parent = node.parent.clone();
        let ram_mb = node.ram_mb;
        match self.spawn_green(&green, &env, &release, profile, depth, parent, ram_mb) {
            Ok(pid) => {
                self.emit_env(
                    &env,
                    "kernel.green",
                    json!({"id": id, "green": green, "pid": pid, "release": release}),
                );
                let _ = reply.send(Ok(json!({"green": green, "pid": pid, "release": release})));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_green(
        &mut self,
        green: &str,
        env: &str,
        release: &Path,
        profile: String,
        depth: u32,
        parent: Option<String>,
        ram_mb: u64,
    ) -> Result<i32, String> {
        self.generation += 1;
        let generation = self.generation;
        let token = crate::token::generate();
        let runtime_token = crate::token::generate();
        let gateway = self.home.green_gateway_address(env);
        let _ = std::fs::remove_file(self.home.green_gateway_sock(env));
        let mut spec = node::spec(
            &self.config,
            &self.home,
            "agent",
            green,
            token.clone(),
            profile.clone(),
            gateway.clone(),
            self.probes.joined(),
        );
        // The exit of A' is the env's exit, so base supervises it normally the
        // moment it becomes A; while it is still green the generation does not
        // match and the exit is ignored, which is what keeps a failed candidate
        // from restarting anything.
        spec.exit_env = env.to_string();
        let running = node::spawn(
            &spec,
            &self.config,
            &self.home,
            release,
            &self.privilege,
            generation,
            self.exits.clone(),
        )
        .map_err(|error| error.to_string())?;
        let mut node = Node::staged("agent", parent, depth, profile, ram_mb);
        node.pid = Some(running.pid);
        node.generation = generation;
        node.exited = running.exited;
        node.token = token;
        node.runtime_token = runtime_token;
        node.shadow = true;
        node.release = release.to_path_buf();
        node.gateway = Some(gateway);
        self.nodes.insert(green.to_string(), node);
        Ok(running.pid)
    }

    /// The switch itself: A' takes the env's name, its sandbox, its state file
    /// and its budget; A is drained. The worker comes back against A''s
    /// gateway and the harness with it, so the sessions in the log answer
    /// again on the new kernel.
    pub fn kernel_ready(
        &mut self,
        id: i64,
        env: String,
        outcome: Result<(), String>,
        reply: oneshot::Sender<Answer>,
    ) {
        let green = green_of(&env);
        if let Err(reason) = outcome {
            self.drop_green(&green);
            self.emit_env(&env, "kernel.rejected", json!({"id": id, "reason": reason}));
            let _ = reply.send(Err(reason));
            return;
        }
        let Some(fresh) = self.nodes.remove(&green) else {
            let _ = reply.send(Err(format!("the new kernel node for {env} is gone")));
            return;
        };
        let Some(mut old) = self.nodes.remove(&env) else {
            let _ = reply.send(Err(format!("unknown env {env}")));
            return;
        };
        if let Some(pid) = old.pid.take() {
            self.draining.insert(env.clone(), (pid, old.exited.take()));
        }
        let (pid, release, gateway) = (fresh.pid, fresh.release.clone(), fresh.gateway.clone());
        let node = merge(old, fresh);
        let runtime_token = node.runtime_token.clone();
        self.nodes.insert(env.clone(), node);
        self.runtimes.remove(&env);
        self.write_runtime_token(&env, &runtime_token);
        let _ = self.home.write_kernel_release(&env, &release);
        self.emit_env(
            &env,
            "kernel.promote",
            json!({"id": id, "pid": pid, "release": release, "gateway": gateway}),
        );
        let _ = self.cmds.send(Cmd::KernelDrain { env: env.clone() });
        let _ = self.cmds.send(Cmd::WorkerBoot { env: env.clone() });
        let _ = reply.send(Ok(json!({
            "ok": true,
            "env": env,
            "pid": pid,
            "release": release,
            "gateway": gateway,
        })));
    }

    /// A is stopped after the front door has moved, not before: the drain is
    /// what closes the old worker's and the old harness's connections, and both
    /// are already coming back on A'.
    pub async fn kernel_drain(&mut self, env: &str) {
        let grace = Duration::from_millis(self.config.stop_grace_ms);
        let Some(drain) = self.draining.remove(env) else {
            return;
        };
        self.harness_halt(env, grace).await;
        node::terminate(drain.0, drain.1, grace).await;
        self.emit_env(env, "kernel.drained", json!({"pid": drain.0}));
        if let Some(node) = self.nodes.get_mut(env) {
            node.harness = crate::harness::State::Off;
            node.harness_pid = None;
        }
    }

    fn drop_green(&mut self, green: &str) {
        let Some(mut node) = self.nodes.remove(green) else {
            return;
        };
        let pid = node.pid.take();
        let exited = node.exited.take();
        let grace = Duration::from_millis(self.config.stop_grace_ms);
        tokio::spawn(async move {
            if let Some(pid) = pid {
                node::terminate(pid, exited, grace).await;
            }
        });
    }
}

/// A' inherits the env: everything the sandbox, the state file and the history
/// hold stays, and only what makes a node a node is replaced.
fn merge(mut old: Node, fresh: Node) -> Node {
    Node {
        role: old.role.clone(),
        pid: fresh.pid,
        generation: fresh.generation,
        registered: fresh.registered,
        restarts: old.restarts,
        peer: fresh.peer,
        sandbox: old.sandbox.take(),
        exited: fresh.exited,
        token: fresh.token,
        runtime_token: fresh.runtime_token,
        parent: old.parent.clone(),
        depth: old.depth,
        profile: old.profile.clone(),
        ram_mb: old.ram_mb,
        worker: WorkerState::Off,
        harness: crate::harness::State::Off,
        harness_pid: old.harness_pid,
        harness_restarts: old.harness_restarts,
        harness_exited: old.harness_exited.take(),
        store: old.store.take(),
        fiber: old.fiber.take(),
        ticker: None,
        restore: old.restore.clone(),
        budget: old.budget.clone(),
        shadow: false,
        release: fresh.release,
        gateway: fresh.gateway,
        worker_spec: old.worker_spec.take(),
    }
}
