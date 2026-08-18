use crate::base::Base;
use crate::rpc::Cmd;
use crate::state::{ticker, WorkerState};
use crate::worker;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::oneshot;

impl Base {
    pub fn worker_boot(&mut self, env: &str) {
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        let (Some(instance), Some(peer)) = (node.sandbox.clone(), node.peer.clone()) else {
            return;
        };
        if matches!(node.worker, WorkerState::Booting | WorkerState::Ready(_)) {
            return;
        }
        node.worker = WorkerState::Booting;
        let timeout = Duration::from_millis(self.config.worker.boot_timeout_ms);
        worker::boot(
            env.to_string(),
            instance,
            peer,
            self.home.gateway_sock(env),
            timeout,
            self.cmds.clone(),
        );
        self.emit("worker.boot", Some(env), json!({"ok": true}));
    }

    pub fn worker_ready(&mut self, env: &str, pid: Option<i64>, error: Option<String>) {
        let interval = Duration::from_millis(self.config.worker.pull_interval_ms);
        let cmds = self.cmds.clone();
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        match error {
            Some(reason) => {
                node.worker = WorkerState::Failed(reason.clone());
                self.emit("worker.failed", Some(env), json!({"error": reason}));
            }
            None => {
                node.worker = WorkerState::Ready(pid);
                node.ticker = Some(ticker(env.to_string(), interval, cmds));
                self.emit("worker.ready", Some(env), json!({"pid": pid}));
                self.apply_restore(env);
            }
        }
    }

    /// Pulls whatever the worker has committed since the step this env's state
    /// file last stored. Everything but the bookkeeping runs off the actor: a
    /// pull is a wire round trip into the sandbox plus a file read.
    pub fn snap_pull(&mut self, env: &str, reply: Option<oneshot::Sender<Result<Value, String>>>) {
        let Some(node) = self.nodes.get(env) else {
            answer(reply, Err(format!("unknown env {env}")));
            return;
        };
        let (Some(peer), Some(instance)) = (node.peer.clone(), node.sandbox.clone()) else {
            answer(reply, Err(format!("env {env} has no worker")));
            return;
        };
        if !matches!(node.worker, WorkerState::Ready(_)) {
            answer(reply, Err(format!("env {env} has no worker")));
            return;
        }
        let since = node
            .store
            .as_ref()
            .and_then(|store| store.last_pack_step().ok())
            .unwrap_or(0);
        let workspace = self.home.workspace_dir(env);
        let guest = instance.workspace_path();
        let cmds = self.cmds.clone();
        let name = env.to_string();
        tokio::spawn(async move {
            let outcome = worker::pull(&peer, since, &workspace, &guest).await;
            let result = match outcome {
                Err(error) => Err(error),
                Ok(None) => Ok(json!({"ok": true, "step": since, "pulled": 0})),
                Ok(Some(pulled)) => {
                    let bytes = pulled.bytes.len();
                    let step = pulled.step;
                    let _ = cmds.send(Cmd::SnapPacked {
                        env: name,
                        step,
                        reference: pulled.reference.clone(),
                        bytes: pulled.bytes,
                    });
                    Ok(json!({
                        "ok": true,
                        "step": step,
                        "ref": pulled.reference,
                        "bytes": bytes,
                        "pulled": 1,
                    }))
                }
            };
            answer(reply, result);
        });
    }

    pub fn snap_list(&self, env: &str) -> Result<Value, String> {
        let Some(node) = self.nodes.get(env) else {
            return Err(format!("unknown env {env}"));
        };
        let Some(store) = node.store.as_ref() else {
            return Err(format!("env {env} has no state file"));
        };
        let rows: Vec<Value> = store
            .packs()
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|row| {
                json!({
                    "step": row.step,
                    "ref": row.reference,
                    "bytes": row.bytes.len(),
                    "created_at": row.created_at,
                })
            })
            .collect();
        Ok(json!({"env": env, "count": rows.len(), "packs": rows}))
    }

    pub fn snap_packed(&mut self, env: &str, step: i64, reference: &str, bytes: &[u8]) {
        let keep = self.config.worker.keep_packs;
        let Some(node) = self.nodes.get(env) else {
            return;
        };
        let Some(store) = node.store.as_ref() else {
            return;
        };
        if let Err(error) = store.put_pack(step, reference, bytes) {
            self.emit(
                "snap.pack_failed",
                Some(env),
                json!({"step": step, "error": error.to_string()}),
            );
            return;
        }
        let pruned = store.prune_packs(keep).unwrap_or(0);
        let count = store.pack_count().unwrap_or(0);
        self.emit(
            "snap.pack",
            Some(env),
            json!({"step": step, "ref": reference, "bytes": bytes.len(),
                   "pruned": pruned, "packs": count}),
        );
    }

    /// Empties the workspace and lays every stored pack back into it, so the
    /// fresh instance's worker can fold them into a new `.tenon-snap` and check
    /// the newest ref out. The host copy is the truth; the guest repo is a cache.
    pub fn stage_restore(&mut self, env: &str) -> Vec<(i64, String)> {
        let rows = match self.nodes.get(env).and_then(|node| node.store.as_ref()) {
            Some(store) => store.packs().unwrap_or_default(),
            None => vec![],
        };
        if let Err(error) = self.home.wipe_workspace(env) {
            self.emit(
                "env.wipe_failed",
                Some(env),
                json!({"error": error.to_string()}),
            );
            return vec![];
        }
        if rows.is_empty() {
            return vec![];
        }
        let dir = self.home.restore_dir(env);
        if let Err(error) = std::fs::create_dir_all(&dir) {
            self.emit(
                "env.wipe_failed",
                Some(env),
                json!({"error": error.to_string()}),
            );
            return vec![];
        }
        let mut staged = Vec::new();
        for row in rows {
            let path = dir.join(format!("{}.pack", row.step));
            if std::fs::write(&path, &row.bytes).is_ok() {
                staged.push((row.step, row.reference));
            }
        }
        self.emit(
            "env.restore_staged",
            Some(env),
            json!({"packs": staged.len()}),
        );
        staged
    }

    fn apply_restore(&mut self, env: &str) {
        let Some(node) = self.nodes.get_mut(env) else {
            return;
        };
        let rows = std::mem::take(&mut node.restore);
        if rows.is_empty() {
            return;
        }
        let (Some(peer), Some(instance)) = (node.peer.clone(), node.sandbox.clone()) else {
            return;
        };
        let head = rows
            .last()
            .map(|(_, reference)| reference.clone())
            .unwrap_or_default();
        let guest = instance.workspace_path();
        let dir = self.home.restore_dir(env);
        let cmds = self.cmds.clone();
        let name = env.to_string();
        tokio::spawn(async move {
            let outcome = worker::apply(&peer, &rows, &guest, &head).await;
            let _ = std::fs::remove_dir_all(&dir);
            let _ = cmds.send(match outcome {
                Ok(result) => Cmd::Restored {
                    env: name,
                    result,
                    error: None,
                },
                Err(error) => Cmd::Restored {
                    env: name,
                    result: Value::Null,
                    error: Some(error),
                },
            });
        });
    }

    pub fn restored(&mut self, env: &str, result: Value, error: Option<String>) {
        match error {
            Some(error) => self.emit("env.restore_failed", Some(env), json!({"error": error})),
            None => self.emit("env.restored", Some(env), result),
        }
    }
}

fn answer(reply: Option<oneshot::Sender<Result<Value, String>>>, outcome: Result<Value, String>) {
    if let Some(reply) = reply {
        let _ = reply.send(outcome);
    }
}
