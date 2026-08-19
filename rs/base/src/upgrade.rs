use crate::base::Base;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::oneshot;

type Answer = Result<Value, String>;

pub const PROPOSED: &str = "proposed";
pub const WAITING: &str = "awaiting_approval";
pub const PROMOTED: &str = "promoted";
pub const ROLLED_BACK: &str = "rolled_back";
pub const CANARY: &str = "canary";

const LIST_LIMIT: i64 = 200;

/// One phase of one proposal, appended to the row as it happens: what the
/// change protocol did, in order, is what an agent reads back when it asks why
/// its upgrade was refused.
pub fn phase(name: &str, ok: bool, data: Value) -> Value {
    json!({"phase": name, "ok": ok, "at": tenon_storage::now(), "data": data})
}

fn targets() -> [&'static str; 4] {
    ["plugin", "worker", "kernel", "config"]
}

impl Base {
    /// `upgrade.propose`: the entry of RFC section 10's one protocol. The row
    /// is written first, the tier decides whether a human sees it before
    /// anything runs, and the driver takes it from there. The caller is never
    /// held: an upgrade outlives one request, so the id and `upgrade.status`
    /// are the answer.
    pub fn upgrade_propose(&mut self, env: &str, params: &Value, reply: oneshot::Sender<Answer>) {
        let target = crate::params::text(params, "target");
        if !targets().contains(&target.as_str()) {
            let _ = reply.send(Err(format!(
                "upgrade.propose needs target one of {:?}, not {target:?}",
                targets()
            )));
            return;
        }
        if !self.nodes.contains_key(env) {
            let _ = reply.send(Err(format!("unknown env {env}")));
            return;
        }
        let artifact = crate::params::object(params, "artifact");
        if !artifact.is_object() {
            let _ = reply.send(Err("upgrade.propose needs an artifact object".to_string()));
            return;
        }
        let notes = crate::params::text(params, "notes");
        let tier = self.config.tiers.of(&target).to_string();
        let status = match tier.as_str() {
            "auto" => PROPOSED,
            _ => WAITING,
        };
        let id = match self
            .store
            .put_upgrade(env, &target, &artifact.to_string(), &notes, status)
        {
            Ok(id) => id,
            Err(error) => {
                let _ = reply.send(Err(error.to_string()));
                return;
            }
        };
        self.emit_env(
            env,
            "upgrade.propose",
            json!({"id": id, "target": target, "tier": tier, "notes": notes}),
        );
        let _ = reply.send(Ok(json!({
            "ok": true,
            "id": id,
            "env": env,
            "target": target,
            "tier": tier,
            "status": status,
        })));
        match status {
            PROPOSED => self.upgrade_start(id),
            _ => self.upgrade_ask(id, env, &target),
        }
    }

    /// An `ask` tier goes through the approvals queue like any other host gate,
    /// and nothing of the proposal runs before the verdict.
    fn upgrade_ask(&mut self, id: i64, env: &str, target: &str) {
        let reason = format!("upgrade {id}: {target} of env {env}");
        let (tx, rx) = oneshot::channel();
        self.gate_request(env, &reason, "upgrade", tx);
        let cmds = self.cmds.clone();
        tokio::spawn(async move {
            let verdict = rx
                .await
                .unwrap_or_else(|_| Err("the approval queue is gone".to_string()));
            let outcome = match verdict {
                Ok(value) if value["status"] == json!("approved") => None,
                Ok(value) => Some(format!(
                    "a human did not approve upgrade {id}: approval {} is {}",
                    value["id"], value["status"]
                )),
                Err(error) => Some(error),
            };
            let _ = cmds.send(Cmd::UpgradeApproved {
                id,
                refused: outcome,
            });
        });
    }

    pub fn upgrade_approved(&mut self, id: i64, refused: Option<String>) {
        match refused {
            None => self.upgrade_start(id),
            Some(reason) => {
                let env = self.upgrade_env(id);
                self.upgrade_update(
                    id,
                    ROLLED_BACK,
                    Some(reason.clone()),
                    phase("gate", false, json!({"reason": reason})),
                );
                if let Some(env) = env {
                    self.emit_env(
                        &env,
                        "upgrade.rollback",
                        json!({"id": id, "reason": reason}),
                    );
                }
            }
        }
    }

    fn upgrade_env(&self, id: i64) -> Option<String> {
        self.store.upgrade(id).ok().flatten().map(|row| row.env)
    }

    /// Everything the driver needs, taken off the actor once: it runs the
    /// protocol as ordinary wire requests and file work, and comes back
    /// through `Cmd`s for the few things only base may do.
    pub fn upgrade_start(&mut self, id: i64) {
        let Ok(Some(row)) = self.store.upgrade(id) else {
            return;
        };
        let env = row.env.clone();
        let Some(node) = self.nodes.get(&env) else {
            self.upgrade_update(
                id,
                ROLLED_BACK,
                Some(format!("unknown env {env}")),
                phase("snapshot", false, json!({})),
            );
            return;
        };
        let Some(peer) = node.peer.clone() else {
            self.upgrade_update(
                id,
                ROLLED_BACK,
                Some(format!("env {env} is not registered")),
                phase("snapshot", false, json!({})),
            );
            return;
        };
        let drive = crate::drive::Drive {
            id,
            env: env.clone(),
            target: row.target.clone(),
            artifact: serde_json::from_str(&row.artifact).unwrap_or_else(|_| json!({})),
            peer,
            cmds: self.cmds.clone(),
            home: self.home.clone(),
            release: node.release.clone(),
            instance: node.sandbox.clone(),
            gateway: self.gateway_address(&env),
            bench: self.config.benchmark.clone(),
            timeout: Duration::from_millis(self.config.request_timeout_ms.max(30_000)),
            worker_timeout: Duration::from_millis(self.config.worker.boot_timeout_ms),
        };
        self.emit_env(
            &env,
            "upgrade.start",
            json!({"id": id, "target": row.target}),
        );
        tokio::spawn(crate::drive::drive(drive));
    }

    /// Every phase transition lands in the row and in that env's log, so the
    /// protocol is replayable from the state file alone.
    pub fn upgrade_update(&mut self, id: i64, status: &str, reason: Option<String>, step: Value) {
        let Ok(Some(row)) = self.store.upgrade(id) else {
            return;
        };
        let mut phases: Vec<Value> = serde_json::from_str(&row.phases).unwrap_or_default();
        if !step.is_null() {
            phases.push(step.clone());
        }
        let body = serde_json::to_string(&phases).unwrap_or_else(|_| "[]".to_string());
        let _ = self.store.set_upgrade(id, status, reason.as_deref(), &body);
        let env = row.env.clone();
        self.emit_env(
            &env,
            "upgrade.phase",
            json!({"id": id, "status": status, "reason": reason, "phase": step}),
        );
    }

    pub fn upgrade_status(&self, id: i64) -> Answer {
        let row = self
            .store
            .upgrade(id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("unknown upgrade {id}"))?;
        Ok(view(&row))
    }

    pub fn upgrade_list(&self, env: Option<&str>, limit: i64) -> Answer {
        let rows = self
            .store
            .upgrades(env, limit.clamp(1, LIST_LIMIT))
            .map_err(|error| error.to_string())?;
        let benchmarks = self
            .store
            .benchmarks(env, limit.clamp(1, LIST_LIMIT))
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "count": rows.len(),
            "upgrades": rows.iter().map(view).collect::<Vec<Value>>(),
            "benchmarks": benchmarks
                .iter()
                .filter_map(|row| serde_json::to_value(row).ok())
                .collect::<Vec<Value>>(),
        }))
    }

    /// The candidate worker of a promoted worker upgrade: base launches this
    /// instead of its own built-in line from now on, and a `None` puts the
    /// built-in fallback back.
    pub fn upgrade_worker_spec(&mut self, env: &str, spec: Option<Value>) {
        if let Some(node) = self.nodes.get_mut(env) {
            node.worker_spec = spec.clone();
        }
        let path = self.home.worker_spec_file(env);
        match &spec {
            Some(spec) => {
                let _ = std::fs::write(&path, spec.to_string());
            }
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        self.emit_env(env, "worker.spec", json!({"spec": spec}));
    }

    pub fn on_upgrade_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::UpgradePropose { env, params, reply } => {
                self.upgrade_propose(&env, &params, reply)
            }
            Cmd::UpgradeStatus { id, reply } => {
                let _ = reply.send(self.upgrade_status(id));
            }
            Cmd::UpgradeList { env, limit, reply } => {
                let _ = reply.send(self.upgrade_list(env.as_deref(), limit));
            }
            Cmd::UpgradeApproved { id, refused } => self.upgrade_approved(id, refused),
            Cmd::UpgradePhase {
                id,
                status,
                reason,
                step,
            } => self.upgrade_update(id, &status, reason, step),
            Cmd::UpgradeWorker { env, spec } => self.upgrade_worker_spec(&env, spec),
            Cmd::KernelSwitch {
                id,
                env,
                release,
                reply,
            } => self.kernel_switch(id, env, release, reply),
            Cmd::KernelReady {
                id,
                env,
                outcome,
                reply,
            } => self.kernel_ready(id, env, outcome, reply),
            _ => {}
        }
    }
}

impl Base {
    /// The benchmark rows of the promotion gate, in the barebone's own state
    /// file: one LKG row per env and label, and one row per candidate pass.
    pub fn upgrade_bench(
        &mut self,
        env: &str,
        label: &str,
        id: i64,
        row: (i64, i64, f64, i64),
        lkg: bool,
    ) -> Answer {
        let stored = self
            .store
            .put_benchmark(env, label, Some(id), row, lkg)
            .map_err(|error| error.to_string())?;
        let previous = match lkg {
            true => None,
            false => self.store.lkg_benchmark(env, label).ok().flatten(),
        };
        self.emit_env(
            env,
            "benchmark",
            json!({
                "id": stored,
                "upgrade": id,
                "label": label,
                "lkg": lkg,
                "tasks": row.0,
                "passed": row.1,
                "success_rate": row.2,
                "cost": row.3,
            }),
        );
        Ok(json!({
            "id": stored,
            "lkg": previous.map(|row| json!({
                "success_rate": row.success_rate,
                "cost": row.cost,
                "tasks": row.tasks,
            })),
        }))
    }
}

fn view(row: &tenon_storage::Upgrade) -> Value {
    json!({
        "id": row.id,
        "env": row.env,
        "target": row.target,
        "status": row.status,
        "artifact": serde_json::from_str::<Value>(&row.artifact).unwrap_or(Value::Null),
        "notes": row.notes,
        "reason": row.reason,
        "phases": serde_json::from_str::<Value>(&row.phases).unwrap_or(Value::Null),
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}
