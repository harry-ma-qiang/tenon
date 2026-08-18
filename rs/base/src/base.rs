use crate::config::Config;
use crate::home::Home;
use crate::node::{self, Exit, GUARDIAN};
use crate::peer::Peer;
use crate::rpc::{Cmd, NodeView, Snapshot};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::{Endpoint, Instance, Sandbox, Spec};
use tenon_storage::Store;
use tokio::sync::{mpsc, oneshot};

const BOOT_ABORT_GRACE_MS: u64 = 300;

struct Node {
    role: String,
    pid: Option<i32>,
    generation: u64,
    registered: bool,
    restarts: u32,
    peer: Option<Peer>,
    sandbox: Option<Arc<dyn Instance>>,
    exited: Option<oneshot::Receiver<Option<i32>>>,
    token: String,
}

pub struct Base {
    home: Home,
    config: Config,
    store: Store,
    release: PathBuf,
    sandbox: Arc<dyn Sandbox>,
    exit_on_detach: bool,
    nodes: BTreeMap<String, Node>,
    subs: BTreeMap<u64, (Peer, Option<String>)>,
    exits: mpsc::UnboundedSender<Exit>,
    generation: u64,
    promoted: bool,
    stopping: bool,
}

fn sandbox_env_passthrough() -> Vec<String> {
    std::env::var("TENON_SANDBOX_ENV")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn endpoint_repr(endpoint: &Endpoint) -> Value {
    match endpoint {
        Endpoint::Direct => json!("direct"),
        Endpoint::Uds(path) => json!(format!("unix:{}", path.display())),
        Endpoint::Tcp(host, port) => json!(format!("tcp:{host}:{port}")),
    }
}

fn wanted(filter: Option<&str>, env: Option<&str>) -> bool {
    match (filter, env) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(filter), Some(env)) => filter == env,
    }
}

impl Base {
    pub fn new(
        home: Home,
        config: Config,
        store: Store,
        release: PathBuf,
        sandbox: Arc<dyn Sandbox>,
        exit_on_detach: bool,
        exits: mpsc::UnboundedSender<Exit>,
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
                    Some(exit) => self.on_exit(exit),
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
        self.start(GUARDIAN, GUARDIAN)?;
        self.start("agent", &root)
    }

    fn start(&mut self, role: &str, env: &str) -> Result<(), String> {
        self.generation += 1;
        let generation = self.generation;
        let token = crate::token::generate();
        let spec = node::spec(&self.config, &self.home, role, env, token.clone());
        let running = node::spawn(
            &spec,
            &self.config,
            &self.home,
            &self.release,
            generation,
            self.exits.clone(),
        )
        .map_err(|error| error.to_string())?;
        let restarts = self.nodes.get(env).map(|node| node.restarts).unwrap_or(0);
        let sandbox = self.enter_sandbox(role, env)?;
        self.nodes.insert(
            env.to_string(),
            Node {
                role: role.to_string(),
                pid: Some(running.pid),
                generation,
                registered: false,
                restarts,
                peer: None,
                sandbox,
                exited: running.exited,
                token,
            },
        );
        let _ = self
            .store
            .put_env(env, role, Some(running.pid as i64), "starting");
        self.emit(
            "node.start",
            Some(env),
            json!({"role": role, "pid": running.pid}),
        );
        Ok(())
    }

    fn enter_sandbox(
        &mut self,
        role: &str,
        env: &str,
    ) -> Result<Option<Arc<dyn Instance>>, String> {
        if role == GUARDIAN {
            return Ok(None);
        }
        if let Some(node) = self.nodes.get_mut(env) {
            if let Some(old) = node.sandbox.take() {
                let _ = old.destroy();
            }
        }
        let spec = Spec {
            env: env.to_string(),
            image: std::env::var("TENON_SANDBOX_IMAGE").ok(),
            workspace: self.home.workspace_dir(env),
            gateway: Some(self.home.gateway_address(env)),
            env_passthrough: sandbox_env_passthrough(),
            policy: Default::default(),
            caps: vec![],
            home_hash: self.home.hash(),
            base_pid: std::process::id() as i32,
        };
        self.sandbox
            .spawn(&spec)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    fn sandbox_exec(
        &mut self,
        env: String,
        cmd: String,
        args: Vec<String>,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<Value, String>>,
    ) {
        let Some(instance) = self.nodes.get(&env).and_then(|node| node.sandbox.clone()) else {
            let _ = reply.send(Err(format!("env {env} has no sandbox instance")));
            return;
        };
        tokio::task::spawn_blocking(move || {
            let outcome = instance.exec(&cmd, &args, Duration::from_millis(timeout_ms.max(1)));
            let result = outcome
                .map(|outcome| {
                    json!({
                        "status": outcome.status,
                        "stdout": String::from_utf8_lossy(&outcome.stdout),
                        "stderr": String::from_utf8_lossy(&outcome.stderr),
                        "timed_out": outcome.timed_out,
                    })
                })
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
        });
    }

    fn sandbox_destroy(&mut self, env: &str, reply: oneshot::Sender<Result<Value, String>>) {
        let Some(node) = self.nodes.get_mut(env) else {
            let _ = reply.send(Err(format!("unknown env {env}")));
            return;
        };
        let Some(instance) = node.sandbox.take() else {
            let _ = reply.send(Err(format!("env {env} has no sandbox instance")));
            return;
        };
        self.emit("sandbox.destroy", Some(env), json!({"id": instance.id()}));
        tokio::task::spawn_blocking(move || {
            let _ = instance.destroy();
        });
        let _ = reply.send(Ok(json!({"ok": true})));
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

    fn on_exit(&mut self, exit: Exit) {
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
        let restarts = node.restarts;
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
        if let Err(error) = self.start(&role, &exit.env) {
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
        self.emit("env.reset", Some(env), json!({"pid": pid}));
        if let Some(pid) = pid {
            let grace = Duration::from_millis(self.config.stop_grace_ms);
            node::terminate(pid, exited, grace).await;
        }
        let restored = self.home.restore_env(env).unwrap_or(false);
        self.start(&role, env)?;
        if let Some(node) = self.nodes.get_mut(env) {
            node.restarts = 0;
        }
        let fresh = self.nodes.get(env).and_then(|node| node.pid);
        Ok(json!({"ok": true, "env": env, "pid": fresh, "lkg": restored}))
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
        let order: Vec<String> = self
            .nodes
            .keys()
            .filter(|env| *env != GUARDIAN)
            .cloned()
            .chain(std::iter::once(GUARDIAN.to_string()))
            .collect();
        for env in order {
            let Some(node) = self.nodes.get_mut(&env) else {
                continue;
            };
            let pid = node.pid.take();
            let exited = node.exited.take();
            node.registered = false;
            node.peer = None;
            if let Some(pid) = pid {
                node::terminate(pid, exited, grace).await;
            }
            if let Some(instance) = node.sandbox.take() {
                let _ = instance.destroy();
            }
            let _ = self.store.put_env(&env, &node.role, None, "stopped");
        }
        let _ = self.store.checkpoint();
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

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            home: self.home.root.clone(),
            release: self.release.clone(),
            pid: std::process::id(),
            exit_on_detach: self.exit_on_detach,
            attached: self.subs.len(),
            nodes: self
                .nodes
                .iter()
                .map(|(env, node)| NodeView {
                    env: env.clone(),
                    role: node.role.clone(),
                    pid: node.pid,
                    registered: node.registered,
                    restarts: node.restarts,
                    sandbox: node.sandbox.as_ref().map(|instance| {
                        json!({
                            "backend": instance.backend(),
                            "id": instance.id(),
                            "attach": endpoint_repr(&instance.attach_addr()),
                        })
                    }),
                    peer: node.peer.clone(),
                })
                .collect(),
        }
    }

    fn emit(&mut self, kind: &str, env: Option<&str>, data: Value) {
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
