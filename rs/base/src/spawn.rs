use crate::base::Base;
use crate::envfiber;
use crate::node::GUARDIAN;
use crate::rpc::Cmd;
use crate::state::{Node, WorkerState};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::oneshot;

impl Base {
    /// The soft limit of RFC section 5: below it a runtime spawns itself,
    /// above it the host asks a human first. The reply is held until the
    /// verdict and the spawn resumes as the same command, already approved.
    pub fn on_spawn(
        &mut self,
        peer: u64,
        parent: Option<String>,
        overrides: Value,
        approved: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    ) {
        let total = self
            .nodes
            .values()
            .filter(|node| node.role != GUARDIAN)
            .count();
        let soft = self.config.approval.spawn_soft_limit;
        if approved || soft == 0 || total < soft {
            let outcome = self.spawn_child(peer, parent, &overrides);
            let _ = reply.send(outcome);
            return;
        }
        let env = parent
            .clone()
            .or_else(|| self.env_of_peer(peer))
            .unwrap_or_else(|| self.config.root_env.clone());
        let reason = format!(
            "runtime.spawn from {env}: {total} environments is past the soft limit of {soft}"
        );
        self.gate(
            &env.clone(),
            "runtime.spawn",
            &reason,
            reply,
            move |reply| Cmd::Spawn {
                peer,
                parent,
                overrides,
                approved: true,
                reply,
            },
        );
    }

    /// The barebone is the only thing that creates a runtime. An env asks its
    /// parent (through `Link`), the request lands here, and base builds the
    /// child on the host: its own sandbox instance, node, state file and
    /// workspace, with the parent's profile plus one patch layer as its config.
    pub fn spawn_child(
        &mut self,
        peer: u64,
        parent: Option<String>,
        overrides: &Value,
    ) -> Result<Value, String> {
        let parent = match parent.or_else(|| self.env_of_peer(peer)) {
            Some(name) => name,
            None => return Err("runtime.spawn needs a parent env".to_string()),
        };
        let Some(node) = self.nodes.get(&parent) else {
            return Err(format!("unknown env {parent}"));
        };
        if node.role == GUARDIAN {
            return Err("the guardian spawns nothing".to_string());
        }
        let depth = node.depth + 1;
        let profile = node.profile.clone();
        if depth > self.config.envs.max_depth {
            return Err(format!(
                "depth {depth} is past the limit of {}",
                self.config.envs.max_depth
            ));
        }
        let total = self
            .nodes
            .values()
            .filter(|node| node.role != GUARDIAN)
            .count();
        if total >= self.config.envs.max_total {
            return Err(format!(
                "{total} environments is already the limit of {}",
                self.config.envs.max_total
            ));
        }
        let child = self.child_name(&parent, overrides);
        if self.nodes.contains_key(&child) {
            return Err(format!("env {child} already exists"));
        }
        let ram_mb = overrides
            .get("ram_mb")
            .and_then(Value::as_u64)
            .unwrap_or(self.config.envs.ram_mb);
        let overlay = self
            .home
            .write_overlay(&child, &patch(overrides))
            .map_err(|error| error.to_string())?;
        let layers = format!("{profile}:{}", overlay.display());
        self.stage_child(&child, &parent, depth, layers.clone(), ram_mb);
        if let Err(error) = self.start("agent", &child, Some(parent.clone())) {
            self.nodes.remove(&child);
            return Err(error);
        }
        let fiber = envfiber::mount(
            self.home.gateway_sock(&parent),
            child.clone(),
            self.cmds.clone(),
        );
        let service = fiber.service().to_string();
        if let Some(node) = self.nodes.get_mut(&child) {
            node.fiber = Some(fiber);
        }
        let pid = self.nodes.get(&child).and_then(|node| node.pid);
        self.emit(
            "runtime.spawn",
            Some(&child),
            json!({"parent": parent, "depth": depth, "ram_mb": ram_mb, "service": service}),
        );
        Ok(json!({
            "ok": true,
            "env": child,
            "parent": parent,
            "depth": depth,
            "ram_mb": ram_mb,
            "profile": layers,
            "service": service,
            "pid": pid,
        }))
    }

    pub async fn runtime_stop(&mut self, env: &str) -> Result<Value, String> {
        let Some(node) = self.nodes.get(env) else {
            return Err(format!("unknown env {env}"));
        };
        if node.parent.is_none() {
            return Err(format!("env {env} is not a child environment"));
        }
        let stopped = self.prune(env, true).await;
        Ok(json!({"ok": true, "stopped": stopped}))
    }

    /// Fiber-cascade semantics for the environment tree: a dead parent takes
    /// its whole subtree with it. Reparenting to the grandparent is a later
    /// option (RFC section 4), never a silent one.
    pub async fn prune_children(&mut self, env: &str) {
        let children = self.children_of(env);
        for child in children {
            let stopped = self.prune(&child, true).await;
            self.emit(
                "runtime.prune",
                Some(&child),
                json!({"parent": env, "stopped": stopped}),
            );
        }
    }

    async fn prune(&mut self, env: &str, include_self: bool) -> Vec<String> {
        let mut stopped = Vec::new();
        let children = self.children_of(env);
        for child in children {
            stopped.extend(Box::pin(self.prune(&child, true)).await);
        }
        if !include_self {
            return stopped;
        }
        let grace = Duration::from_millis(self.config.stop_grace_ms);
        self.halt(env, grace).await;
        self.nodes.remove(env);
        let _ = self.store.drop_env(env);
        let _ = std::fs::remove_file(self.home.env_state_file(env));
        stopped.push(env.to_string());
        stopped
    }

    fn stage_child(&mut self, child: &str, parent: &str, depth: u32, profile: String, ram_mb: u64) {
        self.nodes.insert(
            child.to_string(),
            Node {
                role: "agent".to_string(),
                pid: None,
                generation: 0,
                registered: false,
                restarts: 0,
                peer: None,
                sandbox: None,
                exited: None,
                token: String::new(),
                parent: Some(parent.to_string()),
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
            },
        );
    }

    fn child_name(&self, parent: &str, overrides: &Value) -> String {
        if let Some(name) = overrides.get("name").and_then(Value::as_str) {
            if !name.is_empty() {
                return name.to_string();
            }
        }
        let mut seq = 1;
        loop {
            let name = format!("{parent}.{seq}");
            if !self.nodes.contains_key(&name) {
                return name;
            }
            seq += 1;
        }
    }

    pub fn env_of_peer(&self, peer: u64) -> Option<String> {
        self.nodes
            .iter()
            .find(|(_, node)| node.peer.as_ref().map(|peer| peer.id()) == Some(peer))
            .map(|(env, _)| env.clone())
    }
}

fn patch(overrides: &Value) -> String {
    let rows = match overrides.get("patch") {
        Some(Value::Array(rows)) => Value::Array(rows.clone()),
        Some(Value::Null) | None => Value::Array(vec![]),
        Some(other) => Value::Array(vec![other.clone()]),
    };
    serde_yaml::to_string(&rows).unwrap_or_else(|_| "[]\n".to_string())
}
