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
    pub parent: Option<String>,
    pub depth: u32,
    pub profile: String,
    pub ram_mb: u64,
    pub worker: WorkerState,
    pub store: Option<Store>,
    pub fiber: Option<envfiber::Handle>,
    pub ticker: Option<Ticker>,
    pub restore: Vec<(i64, String)>,
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
                    parent: node.parent.clone(),
                    depth: node.depth,
                    children: self.children_of(env),
                    worker: worker_view(&node.worker),
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
