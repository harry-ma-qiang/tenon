use crate::config::Config;
use crate::home::Home;
use crate::node::{self, Exit, GUARDIAN};
use crate::peer::Peer;
use crate::rpc::Cmd;
use crate::state::Node;
use crate::state::WorkerState;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::Sandbox;
use tenon_storage::Store;
use tokio::sync::{mpsc, oneshot};

const BOOT_ABORT_GRACE_MS: u64 = 300;

/// The pid of a node that has been replaced, and the receiver that reports its
/// exit: what a blue/green drain needs to stop the old node after the switch.
pub type Drained = (i32, Option<oneshot::Receiver<Option<i32>>>);

pub struct Base {
    pub home: Home,
    pub config: Config,
    pub store: Store,
    pub release: PathBuf,
    pub sandbox: Arc<dyn Sandbox>,
    pub exit_on_detach: bool,
    pub nodes: BTreeMap<String, Node>,
    /// The connections that hold a live `bus.subscribe` (the UI, `tenon run`,
    /// `tenon attach`). Only their departure counts toward `exit_on_detach`, and
    /// their number is the `attached` figure `status` reports.
    pub attached: std::collections::BTreeSet<u64>,
    pub exits: mpsc::UnboundedSender<Exit>,
    pub cmds: mpsc::UnboundedSender<Cmd>,
    pub generation: u64,
    pub promoted: bool,
    pub stopping: bool,
    pub pending: BTreeMap<i64, crate::approvals::Pending>,
    pub killed: Option<String>,
    pub runtimes: BTreeMap<String, crate::runtime::Runtime>,
    pub probes: crate::probes::Approved,
    pub privilege: crate::privilege::Plan,
    /// The node a blue/green switch has replaced, held between the swap and
    /// the drain so the old process is stopped after the front door moved.
    pub draining: BTreeMap<String, Drained>,
    /// The message hub, when the facades are wired. Every event base or a
    /// producer records is published here once (RFC 8: `session/<kind>` for an
    /// env, `base/<kind>` for the barebone), which is the single fan-out
    /// `bus.subscribe` reads — the P4.0 session bridge and base's own subscriber
    /// list both collapsed into it in P4.1.
    pub hub: Option<std::sync::Arc<tenon_bus::Hub>>,
}

impl Base {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        home: Home,
        config: Config,
        store: Store,
        release: PathBuf,
        sandbox: Arc<dyn Sandbox>,
        exit_on_detach: bool,
        exits: mpsc::UnboundedSender<Exit>,
        cmds: mpsc::UnboundedSender<Cmd>,
    ) -> Self {
        Self {
            home,
            config,
            store,
            release,
            sandbox,
            exit_on_detach,
            nodes: BTreeMap::new(),
            attached: std::collections::BTreeSet::new(),
            exits,
            cmds,
            generation: 0,
            promoted: false,
            stopping: false,
            pending: BTreeMap::new(),
            killed: None,
            runtimes: BTreeMap::new(),
            probes: crate::probes::Approved::default(),
            privilege: crate::privilege::Plan::Off,
            draining: BTreeMap::new(),
            hub: None,
        }
    }

    pub async fn run(
        mut self,
        mut cmds: mpsc::UnboundedReceiver<Cmd>,
        mut exits: mpsc::UnboundedReceiver<Exit>,
    ) -> i32 {
        loop {
            tokio::select! {
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.on_cmd(cmd).await,
                    None => break,
                },
                exit = exits.recv() => match exit {
                    Some(exit) => self.on_exit(exit).await,
                    None => break,
                },
            }
            if self.stopping {
                break;
            }
        }
        let _ = std::fs::remove_file(self.home.ready_file());
        let _ = std::fs::remove_file(self.home.sock());
        0
    }

    /// The address that env's gateway listens on and everything reaching it
    /// dials. Base's own answer is a unix socket per env; a sandbox backend
    /// whose guest cannot see a host socket path — krun — replaces it with a
    /// loopback TCP address, and node A, the harness and the worker are all
    /// handed the same string.
    pub fn gateway_address(&self, env: &str) -> String {
        if let Some(address) = self.nodes.get(env).and_then(|node| node.gateway.clone()) {
            return address;
        }
        let default = self.home.gateway_address(env);
        self.sandbox
            .gateway_address(env, &default)
            .unwrap_or(default)
    }

    pub(crate) fn boot(&mut self) -> Result<(), String> {
        let root = self.config.root_env.clone();
        self.emit(
            "base.boot",
            None,
            json!({"release": self.release, "sandbox": self.sandbox.backend()}),
        );
        self.load_probes();
        self.load_privilege();
        self.start(GUARDIAN, GUARDIAN, None)?;
        self.start("agent", &root, None)
    }

    pub fn start(&mut self, role: &str, env: &str, parent: Option<String>) -> Result<(), String> {
        self.generation += 1;
        let generation = self.generation;
        let token = crate::token::generate();
        let runtime_token = crate::token::generate();
        let depth = parent
            .as_ref()
            .and_then(|name| self.nodes.get(name))
            .map(|node| node.depth + 1)
            .unwrap_or(0);
        let profile = match self.nodes.get(env) {
            Some(node) => node.profile.clone(),
            None => self.home.profile(env).display().to_string(),
        };
        let ram_mb = self
            .nodes
            .get(env)
            .map(|node| node.ram_mb)
            .unwrap_or(self.config.envs.ram_mb);
        self.home
            .prepare_env(env)
            .map_err(|error| error.to_string())?;
        self.hand_over(role, env);
        // A promoted kernel moved this env off base's own release; the
        // profiles hold the pointer, so `tenon rollback` puts the old one back
        // with everything else it restores.
        let release = match role == GUARDIAN {
            true => self.release.clone(),
            false => self
                .nodes
                .get(env)
                .map(|node| node.release.clone())
                .filter(|path| path.join("bin/tenon_beam").is_file())
                .or_else(|| self.home.kernel_release(env))
                .unwrap_or_else(|| self.release.clone()),
        };
        let spec = node::spec(
            &self.config,
            &self.home,
            role,
            env,
            token.clone(),
            profile,
            self.gateway_address(env),
            self.probes.joined(),
        );
        let running = node::spawn(
            &spec,
            &self.config,
            &self.home,
            &release,
            &self.privilege,
            generation,
            self.exits.clone(),
        )
        .map_err(|error| error.to_string())?;
        let mut previous = self.nodes.remove(env);
        let restarts = previous.as_ref().map(|node| node.restarts).unwrap_or(0);
        let mut budget = previous
            .as_ref()
            .map(|node| node.budget.clone())
            .unwrap_or_default();
        if budget.started == 0 {
            budget.started = tenon_storage::now();
        }
        let sandbox = self.enter_sandbox(role, env, previous.as_ref(), ram_mb)?;
        let store = match previous.as_mut().and_then(|old| old.store.take()) {
            Some(store) => Some(store),
            None => self.env_store(role, env),
        };
        let fiber = previous.as_mut().and_then(|old| old.fiber.take());
        let node = Node {
            role: role.to_string(),
            pid: Some(running.pid),
            generation,
            registered: false,
            restarts,
            peer: None,
            sandbox,
            exited: running.exited,
            token,
            runtime_token: runtime_token.clone(),
            parent: previous
                .as_ref()
                .and_then(|old| old.parent.clone())
                .or(parent.clone()),
            depth: previous.as_ref().map(|old| old.depth).unwrap_or(depth),
            profile: spec.profile.clone(),
            ram_mb,
            worker: WorkerState::Off,
            harness: crate::harness::State::Off,
            harness_pid: None,
            harness_restarts: 0,
            harness_exited: None,
            store,
            fiber,
            ticker: None,
            restore: previous
                .as_ref()
                .map(|old| old.restore.clone())
                .unwrap_or_default(),
            budget,
            shadow: false,
            release,
            gateway: previous.as_ref().and_then(|old| old.gateway.clone()),
            worker_spec: previous
                .as_mut()
                .and_then(|old| old.worker_spec.take())
                .or_else(|| self.home.worker_spec(env)),
        };
        let first = previous.is_none() && role != GUARDIAN;
        self.nodes.insert(env.to_string(), node);
        // A fresh sandbox is an empty workspace: on the first start of an env
        // in this base process the stored packs are staged the same way a
        // `reset` stages them, so `start` after `stop` replays the workspace
        // instead of inheriting whatever the last boot left on the host.
        if first {
            let staged = self.stage_restore(env);
            if let Some(node) = self.nodes.get_mut(env) {
                node.restore = staged;
            }
        }
        self.runtimes.remove(env);
        self.write_runtime_token(env, &runtime_token);
        let _ = self
            .store
            .put_env(env, role, Some(running.pid as i64), "starting");
        if let Some(parent) = &parent {
            let _ = self.store.put_env_parent(env, Some(parent), depth as i64);
        }
        self.emit(
            "node.start",
            Some(env),
            json!({"role": role, "pid": running.pid, "parent": parent, "depth": depth}),
        );
        Ok(())
    }

    fn env_store(&mut self, role: &str, env: &str) -> Option<Store> {
        if role == GUARDIAN {
            return None;
        }
        match Store::open(&self.home.env_state_file(env)) {
            Ok(store) => Some(store),
            Err(error) => {
                self.emit(
                    "env.state_failed",
                    Some(env),
                    json!({"error": error.to_string()}),
                );
                None
            }
        }
    }

    pub(crate) fn on_register(
        &mut self,
        peer: Peer,
        role: String,
        env: String,
        pid: i64,
        token: String,
        reply: oneshot::Sender<Result<Value, String>>,
    ) {
        let Some(node) = self.nodes.get_mut(&env) else {
            self.emit(
                "node.register_rejected",
                Some(&env),
                json!({"reason": "unknown_env"}),
            );
            let _ = reply.send(Err("unknown_env".to_string()));
            return;
        };
        if node.token != token || node.pid != Some(pid as i32) {
            self.emit(
                "node.register_rejected",
                Some(&env),
                json!({"role": role, "pid": pid}),
            );
            let _ = reply.send(Err("unauthorized".to_string()));
            return;
        }
        node.peer = Some(peer);
        node.registered = true;
        let _ = self.store.put_env(&env, &role, Some(pid), "up");
        self.emit(
            "node.register",
            Some(&env),
            json!({"role": role, "pid": pid}),
        );
        let _ = reply.send(Ok(json!({"ok": true})));
        let shadow = self
            .nodes
            .get(&env)
            .map(|node| node.shadow)
            .unwrap_or(false);
        if role != GUARDIAN && !shadow {
            let _ = self.cmds.send(Cmd::WorkerBoot { env: env.clone() });
        }
        if self.ready() && !self.promoted {
            self.promoted = true;
            let _ = self.store.checkpoint();
            match self.home.promote_lkg() {
                Ok(()) => {
                    let manifest = crate::manifest::write(&self.home, &self.release.clone());
                    let data = match manifest {
                        Ok(manifest) => json!({"ok": true, "manifest": manifest}),
                        Err(error) => json!({"ok": true, "manifest_error": error.to_string()}),
                    };
                    self.emit("lkg.promote", None, data);
                }
                Err(error) => self.emit("lkg.promote", None, json!({"error": error.to_string()})),
            }
            self.emit("base.ready", None, json!({"nodes": self.nodes.len()}));
        }
    }

    async fn on_exit(&mut self, exit: Exit) {
        if self.stopping {
            return;
        }
        let Some(node) = self.nodes.get_mut(&exit.env) else {
            return;
        };
        if node.generation != exit.generation {
            return;
        }
        let role = node.role.clone();
        node.registered = false;
        node.peer = None;
        node.pid = None;
        node.exited = None;
        node.worker = WorkerState::Off;
        node.ticker = None;
        let restarts = node.restarts;
        self.harness_halt(&exit.env, Duration::from_millis(self.config.stop_grace_ms))
            .await;
        let _ = self.store.put_env(&exit.env, &role, None, "down");
        self.emit(
            "node.exit",
            Some(&exit.env),
            json!({"code": exit.code, "role": role, "restarts": restarts}),
        );
        if role == GUARDIAN {
            eprintln!(
                "tenon base: GUARDIAN NODE DIED (code {:?}), restarting",
                exit.code
            );
        }
        self.prune_children(&exit.env).await;
        if restarts >= self.config.max_restarts {
            self.emit(
                "node.give_up",
                Some(&exit.env),
                json!({"restarts": restarts}),
            );
            return;
        }
        if let Some(node) = self.nodes.get_mut(&exit.env) {
            node.restarts = restarts + 1;
        }
        let _ = self.home.restore_env(&exit.env);
        let parent = self
            .nodes
            .get(&exit.env)
            .and_then(|node| node.parent.clone());
        if let Err(error) = self.start(&role, &exit.env, parent) {
            self.emit(
                "node.start_failed",
                Some(&exit.env),
                json!({"error": error}),
            );
        }
    }

    pub(crate) async fn reset(&mut self, env: &str) -> Result<Value, String> {
        if env == GUARDIAN {
            return Err("the guardian is not resettable".to_string());
        }
        if !self.nodes.contains_key(env) {
            return Err(format!("unknown env {env}"));
        }
        self.ensure_state_integrity();
        let Some(node) = self.nodes.get_mut(env) else {
            return Err(format!("unknown env {env}"));
        };
        let role = node.role.clone();
        let pid = node.pid;
        let exited = node.exited.take();
        node.registered = false;
        node.peer = None;
        node.worker = WorkerState::Off;
        node.ticker = None;
        self.emit("env.reset", Some(env), json!({"pid": pid}));
        let grace = Duration::from_millis(self.config.stop_grace_ms);
        self.harness_halt(env, grace).await;
        if let Some(pid) = pid {
            let grace = Duration::from_millis(self.config.stop_grace_ms);
            node::terminate(pid, exited, grace).await;
        }
        let staged = self.stage_restore(env);
        let restored = self.home.restore_env(env).unwrap_or(false);
        let parent = self.nodes.get(env).and_then(|node| node.parent.clone());
        self.start(&role, env, parent)?;
        let clear = self.config.budget_reset_on_reset;
        if let Some(node) = self.nodes.get_mut(env) {
            node.restarts = 0;
            node.restore = staged.clone();
            if clear {
                node.budget = crate::budget::Budget {
                    started: tenon_storage::now(),
                    ..Default::default()
                };
            }
        }
        if clear {
            self.emit_env(env, "budget.reset", json!({"ok": true}));
        }
        let fresh = self.nodes.get(env).and_then(|node| node.pid);
        Ok(json!({
            "ok": true,
            "env": env,
            "pid": fresh,
            "lkg": restored,
            "packs": staged.len(),
        }))
    }

    fn ensure_state_integrity(&mut self) {
        let path = self.home.state_file();
        let lkg = self.home.lkg_state_file();
        match crate::integrity::restore_if_corrupt(&path, &lkg) {
            Ok(false) => {}
            Ok(true) => match Store::open(&path) {
                Ok(store) => {
                    self.store = store;
                    self.emit("state.restored", None, json!({"from_lkg": lkg.is_file()}));
                }
                Err(error) => self.emit(
                    "state.restore_failed",
                    None,
                    json!({"error": error.to_string()}),
                ),
            },
            Err(error) => self.emit(
                "state.restore_failed",
                None,
                json!({"error": error.to_string()}),
            ),
        }
    }

    pub(crate) async fn stop(&mut self) {
        self.stopping = true;
        self.emit("base.stop", None, json!({"ok": true}));
        self.flush_envs().await;
        self.stop_nodes(Duration::from_millis(self.config.stop_grace_ms))
            .await;
    }

    pub(crate) async fn abort_boot(&mut self) {
        self.stopping = true;
        self.emit(
            "base.stop",
            None,
            json!({"ok": true, "reason": "boot_aborted"}),
        );
        self.stop_nodes(Duration::from_millis(BOOT_ABORT_GRACE_MS))
            .await;
    }

    async fn stop_nodes(&mut self, grace: Duration) {
        let mut deepest: Vec<(u32, String)> = self
            .nodes
            .iter()
            .filter(|(env, _)| *env != GUARDIAN)
            .map(|(env, node)| (node.depth, env.clone()))
            .collect();
        deepest.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut order: Vec<String> = deepest.into_iter().map(|(_, env)| env).collect();
        order.push(GUARDIAN.to_string());
        for env in order {
            self.halt(&env, grace).await;
        }
        let _ = self.store.checkpoint();
    }

    pub async fn halt(&mut self, env: &str, grace: Duration) {
        if !self.nodes.contains_key(env) {
            return;
        }
        self.harness_halt(env, grace).await;
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        let pid = node.pid.take();
        let exited = node.exited.take();
        node.registered = false;
        node.peer = None;
        node.worker = WorkerState::Off;
        node.ticker = None;
        node.fiber = None;
        let role = node.role.clone();
        if let Some(pid) = pid {
            node::terminate(pid, exited, grace).await;
        }
        if let Some(node) = self.nodes.get_mut(env) {
            if let Some(instance) = node.sandbox.take() {
                let _ = instance.destroy();
            }
        }
        let _ = self.store.put_env(env, &role, None, "stopped");
    }

    pub(crate) async fn on_gone(&mut self, peer: u64) {
        let was = self.attached.remove(&peer);
        if was && self.attached.is_empty() && self.exit_on_detach {
            self.emit("base.detach", None, json!({"exit_on_detach": true}));
            self.stop().await;
        }
    }

    pub(crate) fn ready(&self) -> bool {
        !self.nodes.is_empty()
            && self
                .nodes
                .values()
                .all(|node| node.registered || node.shadow)
    }

    pub fn emit(&mut self, kind: &str, env: Option<&str>, data: Value) {
        // The leak scrub is the http feature's; with it off this reduces to the
        // exact original append + fan-out, so the default binary is unchanged.
        #[cfg(feature = "http")]
        let data = {
            let mut data = data;
            if let Some(hub) = &self.hub {
                if hub.scrub(&mut data).is_err() {
                    // A `block` secret: never reaches the state file. The hub
                    // emits the violation when publish_event fans it out.
                    self.publish_event(kind, env, &data);
                    return;
                }
            }
            data
        };
        if self.store.append(kind, env, &data).is_err() {
            return;
        }
        self.publish_event(kind, env, &data);
    }

    /// The single bus fan-out for every event base or a producer records: a
    /// `session/<kind>` topic for an env (so a subscriber can range one env's log
    /// or `session.history`), a `base/<kind>` topic for the barebone. Non-durable
    /// — the durable truth is the state file the caller already wrote to (log =
    /// truth, RFC section 1); the bus is the live delivery on top. `session` and
    /// `step` are lifted out of the payload so a subscriber can filter on them.
    pub fn publish_event(&self, kind: &str, env: Option<&str>, data: &Value) {
        let Some(hub) = &self.hub else {
            return;
        };
        let topic = match env {
            Some(_) => format!("session/{kind}"),
            None => format!("base/{kind}"),
        };
        let mut envelope = tenon_bus::Envelope::new(topic, tenon_bus::Level::Info, data.clone());
        envelope.env = env.map(str::to_string);
        envelope.src = "base".to_string();
        envelope.session = data
            .get("session")
            .and_then(Value::as_str)
            .map(str::to_string);
        envelope.step = data.get("step").and_then(Value::as_i64);
        hub.emit(envelope);
    }
}
