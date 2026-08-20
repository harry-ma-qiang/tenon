use crate::base::Base;
use crate::envfiber;
use crate::instance::endpoint_repr;
use crate::peer::Peer;
use crate::rpc::{Cmd, NodeView, Snapshot};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::Instance;
use tenon_storage::Store;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    Off,
    Booting,
    Ready(Option<i64>),
    Failed(String),
}

pub struct Ticker {
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for Ticker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

pub struct Node {
    pub role: String,
    pub pid: Option<i32>,
    pub generation: u64,
    pub registered: bool,
    pub restarts: u32,
    pub peer: Option<Peer>,
    pub sandbox: Option<Arc<dyn Instance>>,
    pub exited: Option<oneshot::Receiver<Option<i32>>>,
    pub token: String,
    pub runtime_token: String,
    pub parent: Option<String>,
    pub depth: u32,
    pub profile: String,
    pub ram_mb: u64,
    pub worker: WorkerState,
    pub harness: crate::harness::State,
    pub harness_pid: Option<i32>,
    pub harness_restarts: u32,
    pub harness_exited: Option<oneshot::Receiver<Option<i32>>>,
    pub store: Option<Store>,
    pub fiber: Option<envfiber::Handle>,
    pub ticker: Option<Ticker>,
    pub restore: Vec<(i64, String)>,
    pub budget: crate::budget::Budget,
    /// A blue/green candidate node: it is in the map so `tenon status` shows
    /// both nodes during a switch, and out of everything else — no worker, no
    /// harness, no probe, no LKG promotion — until it becomes the env's node.
    pub shadow: bool,
    /// The release this node was started from. Normally base's own; after a
    /// promoted kernel upgrade, that env's staged release.
    pub release: std::path::PathBuf,
    /// The gateway address this node listens on, when it is not the env's
    /// default one: the green node of a switch takes a second socket in the
    /// same directory and keeps it after the promotion.
    pub gateway: Option<String>,
    /// The promoted candidate worker, if any. `None` is the built-in worker,
    /// which is the LKG fallback.
    pub worker_spec: Option<Value>,
}

impl Node {
    /// A node record before anything has been spawned for it: what a child env
    /// is staged as, and what a blue/green candidate starts from.
    pub fn staged(
        role: &str,
        parent: Option<String>,
        depth: u32,
        profile: String,
        ram_mb: u64,
    ) -> Self {
        Self {
            role: role.to_string(),
            pid: None,
            generation: 0,
            registered: false,
            restarts: 0,
            peer: None,
            sandbox: None,
            exited: None,
            token: String::new(),
            runtime_token: String::new(),
            parent,
            depth,
            profile,
            ram_mb,
            worker: WorkerState::Off,
            harness: crate::harness::State::Off,
            harness_pid: None,
            harness_restarts: 0,
            harness_exited: None,
            store: None,
            fiber: None,
            ticker: None,
            restore: Vec::new(),
            budget: crate::budget::Budget::default(),
            shadow: false,
            release: std::path::PathBuf::new(),
            gateway: None,
            worker_spec: None,
        }
    }
}

pub fn worker_view(state: &WorkerState) -> Value {
    match state {
        WorkerState::Off => json!({"state": "off"}),
        WorkerState::Booting => json!({"state": "booting"}),
        WorkerState::Ready(pid) => json!({"state": "ready", "pid": pid}),
        WorkerState::Failed(reason) => json!({"state": "failed", "error": reason}),
    }
}

impl Base {
    pub fn env_status(&self, env: &str) -> Result<Value, String> {
        let Some(node) = self.nodes.get(env) else {
            return Err(format!("unknown env {env}"));
        };
        Ok(json!({
            "env": env,
            "role": node.role,
            "pid": node.pid,
            "registered": node.registered,
            "parent": node.parent,
            "depth": node.depth,
            "worker": worker_view(&node.worker),
            "harness": crate::harness::view(&node.harness, node.harness_restarts),
            "runtime": self.runtime_view(env),
            "budget": self.budget_view(env),
            "children": self.children_of(env),
        }))
    }

    pub fn children_of(&self, env: &str) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.parent.as_deref() == Some(env))
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            killed: self.killed.clone(),
            home: self.home.root.clone(),
            release: self.release.clone(),
            pid: std::process::id(),
            exit_on_detach: self.exit_on_detach,
            attached: self.attached.len(),
            nodes: self
                .nodes
                .iter()
                .map(|(env, node)| NodeView {
                    env: env.clone(),
                    budget: self.budget_view(env),
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
                    parent: node.parent.clone(),
                    depth: node.depth,
                    children: self.children_of(env),
                    worker: worker_view(&node.worker),
                    harness: crate::harness::view(&node.harness, node.harness_restarts),
                    runtime: self.runtime_view(env),
                })
                .collect(),
        }
    }
}

pub fn ticker(env: String, interval: Duration, cmds: mpsc::UnboundedSender<Cmd>) -> Ticker {
    let (stop, mut stopped) = oneshot::channel();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stopped => return,
                _ = tokio::time::sleep(interval) => {
                    if cmds.send(Cmd::SnapPull { env: env.clone(), reply: None }).is_err() {
                        return;
                    }
                }
            }
        }
    });
    Ticker { stop: Some(stop) }
}
