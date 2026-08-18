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

pub struct Base {
    pub home: Home,
    pub config: Config,
    pub store: Store,
    pub release: PathBuf,
    pub sandbox: Arc<dyn Sandbox>,
    pub exit_on_detach: bool,
    pub nodes: BTreeMap<String, Node>,
    pub subs: BTreeMap<u64, (Peer, Option<String>)>,
    pub exits: mpsc::UnboundedSender<Exit>,
    pub cmds: mpsc::UnboundedSender<Cmd>,
    pub generation: u64,
    pub promoted: bool,
    pub stopping: bool,
}

fn wanted(filter: Option<&str>, env: Option<&str>) -> bool {
    match (filter, env) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(filter), Some(env)) => filter == env,
    }
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
            subs: BTreeMap::new(),
            exits,
            cmds,
            generation: 0,
            promoted: false,
            stopping: false,
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

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Boot { reply } => {
                let _ = reply.send(self.boot());
            }
            Cmd::Register {
                peer,
                role,
                env,
                pid,
                token,
                reply,
            } => self.on_register(peer, role, env, pid, token, reply),
            Cmd::Snapshot { reply } => {
                let _ = reply.send(self.snapshot());
            }
            Cmd::PeerOf { env, reply } => {
                let peer = self.nodes.get(&env).and_then(|node| node.peer.clone());
                let _ = reply.send(peer);
            }
            Cmd::Reset { env, reply } => {
                let outcome = self.reset(&env).await;
                let _ = reply.send(outcome);
            }
            Cmd::SandboxExec {
                env,
                cmd,
                args,
                timeout_ms,
                reply,
            } => self.sandbox_exec(env, cmd, args, timeout_ms, reply),
            Cmd::SandboxDestroy { env, reply } => self.sandbox_destroy(&env, reply),
            Cmd::SandboxReaped { count } => {
                self.emit("sandbox.reaped", None, json!({"count": count}));
            }
            Cmd::WorkerBoot { env } => self.worker_boot(&env),
            Cmd::HarnessBoot { .. }
            | Cmd::HarnessReady { .. }
            | Cmd::HarnessExit { .. }
            | Cmd::EventsAppend { .. }
            | Cmd::EventsTail { .. }
            | Cmd::ConfigGet { .. }
            | Cmd::ConfigPatch { .. }
            | Cmd::Approval { .. } => self.on_env_cmd(cmd),
            Cmd::WorkerReady { env, pid, error } => self.worker_ready(&env, pid, error),
            Cmd::SnapPull { env, reply } => self.snap_pull(&env, reply),
            Cmd::SnapList { env, reply } => {
                let _ = reply.send(self.snap_list(&env));
            }
            Cmd::SnapPacked {
                env,
                step,
                reference,
                bytes,
            } => self.snap_packed(&env, step, &reference, &bytes),
            Cmd::Spawn {
                peer,
                parent,
                overrides,
                reply,
            } => {
                let outcome = self.spawn_child(peer, parent, &overrides);
                let _ = reply.send(outcome);
            }
            Cmd::RuntimeStop { env, reply } => {
                let outcome = self.runtime_stop(&env).await;
                let _ = reply.send(outcome);
            }
            Cmd::Restored { env, result, error } => self.restored(&env, result, error),
            Cmd::EnvStatus { env, reply } => {
                let _ = reply.send(self.env_status(&env));
            }
            Cmd::Stop { reply } => {
                // Destroy every env's sandbox instance before answering, so a
                // caller that trusts "ok" and force-kills base a moment later
                // (a test fixture's teardown, a supervisor's own timeout) never
                // races an in-flight `podman stop`/`rm -f` and orphans it.
                self.stop().await;
                let _ = reply.send(Ok(json!({"ok": true})));
            }
            Cmd::AbortBoot { reply } => {
                self.abort_boot().await;
                let _ = reply.send(Ok(json!({"ok": true})));
            }
            Cmd::Subscribe { peer, env, reply } => {
                let last = self.store.last_event_id().unwrap_or(0);
                self.subs.insert(peer.id(), (peer, env.clone()));
                let _ = reply.send(json!({"ok": true, "last_event": last, "env": env}));
            }
            Cmd::Gone { peer } => self.on_gone(peer).await,
            Cmd::Ready { reply } => {
                let _ = reply.send(self.ready());
            }
        }
    }

    fn boot(&mut self) -> Result<(), String> {
        let root = self.config.root_env.clone();
        self.emit(
            "base.boot",
            None,
            json!({"release": self.release, "sandbox": self.sandbox.backend()}),
        );
        self.start(GUARDIAN, GUARDIAN, None)?;
        self.start("agent", &root, None)
    }

    pub fn start(&mut self, role: &str, env: &str, parent: Option<String>) -> Result<(), String> {
        self.generation += 1;
        let generation = self.generation;
        let token = crate::token::generate();
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
        let spec = node::spec(&self.config, &self.home, role, env, token.clone(), profile);
        let running = node::spawn(
            &spec,
            &self.config,
            &self.home,
            &self.release,
            generation,
            self.exits.clone(),
        )
        .map_err(|error| error.to_string())?;
        let mut previous = self.nodes.remove(env);
        let restarts = previous.as_ref().map(|node| node.restarts).unwrap_or(0);
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
        };
        self.nodes.insert(env.to_string(), node);
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

    fn on_register(
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
        if role != GUARDIAN {
            let _ = self.cmds.send(Cmd::WorkerBoot { env: env.clone() });
        }
        if self.ready() && !self.promoted {
            self.promoted = true;
            let _ = self.store.checkpoint();
            match self.home.promote_lkg() {
                Ok(()) => self.emit("lkg.promote", None, json!({"ok": true})),
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

    async fn reset(&mut self, env: &str) -> Result<Value, String> {
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
        if let Some(node) = self.nodes.get_mut(env) {
            node.restarts = 0;
            node.restore = staged.clone();
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

    async fn stop(&mut self) {
        self.stopping = true;
        self.emit("base.stop", None, json!({"ok": true}));
        self.stop_nodes(Duration::from_millis(self.config.stop_grace_ms))
            .await;
    }

    async fn abort_boot(&mut self) {
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

    async fn on_gone(&mut self, peer: u64) {
        let was = self.subs.remove(&peer).is_some();
        if was && self.subs.is_empty() && self.exit_on_detach {
            self.emit("base.detach", None, json!({"exit_on_detach": true}));
            self.stop().await;
        }
    }

    fn ready(&self) -> bool {
        !self.nodes.is_empty() && self.nodes.values().all(|node| node.registered)
    }

    pub fn emit(&mut self, kind: &str, env: Option<&str>, data: Value) {
        let Ok(event) = self.store.append(kind, env, &data) else {
            return;
        };
        let frame = json!({
            "t": "event",
            "id": event.id,
            "at": event.at,
            "kind": event.kind,
            "env": event.env,
            "data": event.data,
        });
        for (peer, filter) in self.subs.values() {
            if wanted(filter.as_deref(), event.env.as_deref()) {
                peer.send(frame.clone());
            }
        }
    }
}
