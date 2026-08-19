use crate::base::Base;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::time::Duration;
use tenon_storage::approvals::{APPROVED, DENIED, EXPIRED, PENDING};
use tokio::sync::oneshot;

type Answer = Result<Value, String>;

pub const AUTO: &str = "auto";
pub const DENY: &str = "deny";

const LIST_LIMIT: i64 = 500;

/// One request waiting for a human. The row lives in the barebone's state
/// file (the queue `tenon approvals` reads) and in the env's own file (that
/// env's history); the waiters are the calls blocked on the verdict.
pub struct Pending {
    pub env: String,
    pub env_row: Option<i64>,
    pub kind: String,
    pub reason: String,
    pub waiters: Vec<oneshot::Sender<Answer>>,
}

impl Base {
    /// The mode a **gate** resolves through: base's config and nothing else.
    /// An env may loosen its own `approval.request` (that is the agent asking
    /// for itself) but never the host's gate on a host-affecting action —
    /// otherwise a child's overlay patch would be a way past the gate.
    pub fn gate_mode(&self) -> (String, u64) {
        (
            self.config.approval.mode.clone(),
            self.config.approval.timeout_s,
        )
    }

    /// `ask` from the env's overlay or base's config, with `auto` and `deny`
    /// as the two answers that need no human. This is the agent's own
    /// `approval.request`, not a gate.
    pub fn approval_mode(&self, env: &str) -> (String, u64) {
        let config = &self.config.approval;
        let overlay = self.home.harness_config(env).unwrap_or_else(|_| json!({}));
        match overlay.get("approval") {
            Some(Value::String(mode)) => (mode.clone(), config.timeout_s),
            Some(rows @ Value::Object(_)) => (
                crate::params::text_or(rows, "mode", &config.mode),
                crate::params::u64_or(rows, "timeout_s", config.timeout_s),
            ),
            _ => (config.mode.clone(), config.timeout_s),
        }
    }

    /// The queue's only entry point: `auto` and `deny` answer at once, `ask`
    /// writes a pending row, tells the guardian and holds the caller until a
    /// human answers or the row expires.
    pub fn approval_request(
        &mut self,
        env: &str,
        reason: &str,
        kind: &str,
        reply: oneshot::Sender<Answer>,
    ) {
        self.decide(env, reason, kind, self.approval_mode(env), reply)
    }

    /// A gate asks with base's mode; everything else about the row is the same.
    pub fn gate_request(
        &mut self,
        env: &str,
        reason: &str,
        kind: &str,
        reply: oneshot::Sender<Answer>,
    ) {
        self.decide(env, reason, kind, self.gate_mode(), reply)
    }

    fn decide(
        &mut self,
        env: &str,
        reason: &str,
        kind: &str,
        (mode, timeout_s): (String, u64),
        reply: oneshot::Sender<Answer>,
    ) {
        self.sweep_approvals();
        match mode.as_str() {
            AUTO => {
                let id = self.record(env, reason, kind, APPROVED, None);
                self.emit_env(
                    env,
                    "approval.decided",
                    json!({"id": id, "status": APPROVED, "kind": kind, "auto": true}),
                );
                let _ = reply.send(Ok(
                    json!({"id": id, "status": APPROVED, "auto": true, "reason": reason}),
                ));
            }
            DENY => {
                let id = self.record(env, reason, kind, DENIED, None);
                self.emit_env(
                    env,
                    "approval.decided",
                    json!({"id": id, "status": DENIED, "kind": kind, "auto": true}),
                );
                let _ = reply.send(Ok(json!({
                    "id": id,
                    "status": DENIED,
                    "auto": true,
                    "reason": "approval mode is deny",
                })));
            }
            _ => self.enqueue(env, reason, kind, timeout_s, reply),
        }
    }

    fn enqueue(
        &mut self,
        env: &str,
        reason: &str,
        kind: &str,
        timeout_s: u64,
        reply: oneshot::Sender<Answer>,
    ) {
        let id = self.record(env, reason, kind, PENDING, None);
        let Some(id) = id else {
            let _ = reply.send(Err("the approval queue is not writable".to_string()));
            return;
        };
        let env_row = self.env_approval(env, reason, kind, PENDING);
        self.pending.insert(
            id,
            Pending {
                env: env.to_string(),
                env_row,
                kind: kind.to_string(),
                reason: reason.to_string(),
                waiters: vec![reply],
            },
        );
        self.emit_env(
            env,
            "approval.pending",
            json!({"id": id, "reason": reason, "kind": kind, "timeout_s": timeout_s}),
        );
        self.notify_guardian(
            "approval.pending",
            json!({"id": id, "env": env, "reason": reason, "kind": kind}),
        );
        let cmds = self.cmds.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(timeout_s.max(1))).await;
            let _ = cmds.send(Cmd::ApprovalExpire { id });
        });
    }

    pub fn approval_list(&mut self, status: Option<&str>, limit: i64) -> Answer {
        self.sweep_approvals();
        let rows = self
            .store
            .approvals(status, limit.clamp(1, LIST_LIMIT))
            .map_err(|error| error.to_string())?;
        let rows: Vec<Value> = rows
            .iter()
            .filter_map(|row| serde_json::to_value(row).ok())
            .collect();
        Ok(json!({"count": rows.len(), "approvals": rows}))
    }

    /// A human's verdict: the row in both files moves, the blocked calls are
    /// released with it, and the event log keeps what was decided and why.
    pub fn approval_answer(&mut self, id: i64, decision: &str, note: Option<&str>) -> Answer {
        let status = match decision {
            "deny" | "denied" | "no" | "false" => DENIED,
            _ => APPROVED,
        };
        let moved = self
            .store
            .decide_approval(id, status, note)
            .map_err(|error| error.to_string())?;
        if !moved {
            let row = self.store.approval(id).map_err(|error| error.to_string())?;
            return match row {
                Some(row) => Err(format!("approval {id} is already {}", row.status)),
                None => Err(format!("unknown approval {id}")),
            };
        }
        let pending = self.pending.remove(&id);
        let env = pending
            .as_ref()
            .map(|row| row.env.clone())
            .or_else(|| self.store.approval(id).ok().flatten().map(|row| row.env));
        if let (Some(row), Some(env)) = (&pending, env.as_deref()) {
            if let Some(env_row) = row.env_row {
                if let Ok(store) = self.env_store_of(env) {
                    let _ = store.decide_approval(env_row, status, note);
                }
            }
        }
        match env.as_deref() {
            Some(env) => self.emit_env(
                env,
                "approval.decided",
                json!({"id": id, "status": status, "note": note}),
            ),
            None => self.emit(
                "approval.decided",
                None,
                json!({"id": id, "status": status, "note": note}),
            ),
        }
        let answer = json!({
            "id": id,
            "status": status,
            "note": note,
            "reason": match status == APPROVED {
                true => pending.as_ref().map(|row| row.reason.clone()).unwrap_or_default(),
                false => format!("a human denied approval {id}"),
            },
        });
        if let Some(row) = pending {
            for waiter in row.waiters {
                let _ = waiter.send(Ok(answer.clone()));
            }
        }
        Ok(json!({"ok": true, "id": id, "status": status}))
    }

    /// The one state transition that happens without a human.
    pub fn approval_expire(&mut self, id: i64) {
        let Some(row) = self.pending.remove(&id) else {
            return;
        };
        let _ = self.store.decide_approval(id, EXPIRED, None);
        if let Some(env_row) = row.env_row {
            if let Ok(store) = self.env_store_of(&row.env) {
                let _ = store.decide_approval(env_row, EXPIRED, None);
            }
        }
        self.emit_env(
            &row.env.clone(),
            "approval.expired",
            json!({"id": id, "kind": row.kind}),
        );
        let answer = json!({
            "id": id,
            "status": EXPIRED,
            "reason": format!("approval {id} expired before a human answered"),
        });
        for waiter in row.waiters {
            let _ = waiter.send(Ok(answer.clone()));
        }
    }

    /// Rows left pending by an earlier base (nothing holds their callers any
    /// more) become `expired` rather than staying in the queue forever.
    pub fn sweep_approvals(&mut self) {
        let older = (self.config.approval.timeout_s.max(1) * 1_000) as i64;
        let Ok(rows) = self.store.approvals(Some(PENDING), LIST_LIMIT) else {
            return;
        };
        let now = tenon_storage::now();
        for row in rows {
            if self.pending.contains_key(&row.id) {
                continue;
            }
            if now - row.created_at >= older {
                let _ = self.store.decide_approval(row.id, EXPIRED, None);
            }
        }
    }

    /// A host-affecting action behind a human gate: the caller's reply is held
    /// until the verdict, and an approval resumes it as the same command with
    /// its gate already passed.
    pub fn gate(
        &mut self,
        env: &str,
        kind: &str,
        reason: &str,
        reply: oneshot::Sender<Answer>,
        resume: impl FnOnce(oneshot::Sender<Answer>) -> Cmd + Send + 'static,
    ) {
        let (tx, rx) = oneshot::channel();
        self.gate_request(env, reason, kind, tx);
        let cmds = self.cmds.clone();
        let kind = kind.to_string();
        tokio::spawn(async move {
            let verdict = rx
                .await
                .unwrap_or_else(|_| Err("the approval queue is gone".to_string()));
            match verdict {
                Ok(value) if value["status"] == json!(APPROVED) => {
                    let _ = cmds.send(resume(reply));
                }
                Ok(value) => {
                    let _ = reply.send(Err(format!(
                        "{kind} needs a human: approval {} is {}",
                        value["id"], value["status"]
                    )));
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        });
    }

    fn record(
        &mut self,
        env: &str,
        reason: &str,
        kind: &str,
        status: &str,
        note: Option<&str>,
    ) -> Option<i64> {
        let id = self.store.put_approval(env, reason, kind, status).ok();
        if status != PENDING {
            self.env_approval(env, reason, kind, status);
        }
        if let (Some(id), Some(note)) = (id, note) {
            let _ = self.store.decide_approval(id, status, Some(note));
        }
        id
    }

    fn env_approval(&self, env: &str, reason: &str, kind: &str, status: &str) -> Option<i64> {
        self.env_store_of(env)
            .ok()
            .and_then(|store| store.put_approval(env, reason, kind, status).ok())
    }

    pub fn env_store_of(&self, env: &str) -> Result<&tenon_storage::Store, String> {
        self.nodes
            .get(env)
            .and_then(|node| node.store.as_ref())
            .ok_or_else(|| format!("env {env} has no state file"))
    }

    /// G is told, it does not own the queue: the guardian's window on the
    /// barebone is a notification, and an unknown frame is not an error worth
    /// failing a request over.
    pub fn notify_guardian(&self, kind: &str, data: Value) {
        let Some(peer) = self
            .nodes
            .get(crate::node::GUARDIAN)
            .and_then(|node| node.peer.clone())
        else {
            return;
        };
        let timeout = Duration::from_millis(self.config.request_timeout_ms);
        let kind = kind.to_string();
        tokio::spawn(async move {
            let _ = peer
                .request("notify", json!({"kind": kind, "data": data}), timeout)
                .await;
        });
    }

    /// Workspace push-out is a host-affecting action, so it is gated unless
    /// base's config turns the gate off.
    pub fn on_snap_export(
        &mut self,
        env: String,
        path: String,
        approved: bool,
        reply: oneshot::Sender<Answer>,
    ) {
        if approved || !self.config.approval.gate_snap_export {
            let _ = reply.send(self.snap_export(&env, &path));
            return;
        }
        let reason = format!("snap.export of {env} to the host path {path}");
        let name = env.clone();
        self.gate(&name, "snap.export", &reason, reply, move |reply| {
            Cmd::SnapExport {
                env,
                path,
                approved: true,
                reply,
            }
        });
    }

    /// The workspace push-out of RFC section 8: the newest pack the host holds
    /// for that env, written to a host path as a self-contained bundle.
    pub fn snap_export(&mut self, env: &str, path: &str) -> Answer {
        if path.is_empty() {
            return Err("snap.export needs a path".to_string());
        }
        let store = self.env_store_of(env)?;
        let rows = store.packs().map_err(|error| error.to_string())?;
        let Some(newest) = rows.into_iter().max_by_key(|row| row.step) else {
            return Err(format!("env {env} has no snapshot to export"));
        };
        let target = std::path::PathBuf::from(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(&target, &newest.bytes).map_err(|error| error.to_string())?;
        let answer = json!({
            "ok": true,
            "env": env,
            "step": newest.step,
            "ref": newest.reference,
            "bytes": newest.bytes.len(),
            "path": target,
        });
        self.emit_env(env, "snap.export", answer.clone());
        Ok(answer)
    }
}
