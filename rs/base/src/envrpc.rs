use crate::base::Base;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::oneshot;

pub const APPROVAL_OFF: &str = "approvals not enabled";

type Answer = Result<Value, String>;

impl Base {
    /// The session log lives in the env's own state file and base is its only
    /// writer, so the harness appends through this call instead of opening
    /// sqlite itself. Every appended event also reaches the subscribers of
    /// `tenon attach`, which is what makes `tenon run` streamable.
    pub fn events_append(&mut self, env: &str, kind: &str, data: &Value) -> Answer {
        let Some(node) = self.nodes.get(env) else {
            return Err(format!("unknown env {env}"));
        };
        let Some(store) = node.store.as_ref() else {
            return Err(format!("env {env} has no state file"));
        };
        let event = store
            .append(kind, Some(env), data)
            .map_err(|error| error.to_string())?;
        let frame = json!({
            "t": "event",
            "id": event.id,
            "at": event.at,
            "kind": event.kind,
            "env": event.env,
            "scope": "env",
            "data": event.data,
        });
        for (peer, filter) in self.subs.values() {
            if filter.is_none() || filter.as_deref() == Some(env) {
                peer.send(frame.clone());
            }
        }
        Ok(json!({"id": event.id, "at": event.at}))
    }

    pub fn events_tail(&self, env: &str, after: i64, limit: i64) -> Answer {
        let Some(node) = self.nodes.get(env) else {
            return Err(format!("unknown env {env}"));
        };
        let Some(store) = node.store.as_ref() else {
            return Err(format!("env {env} has no state file"));
        };
        let events = store
            .events_since(after, limit.clamp(1, 20_000))
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "env": env,
            "count": events.len(),
            "events": events
                .iter()
                .map(|event| json!({
                    "id": event.id,
                    "at": event.at,
                    "kind": event.kind,
                    "data": event.data,
                }))
                .collect::<Vec<Value>>(),
        }))
    }

    pub fn config_get(&self, env: &str) -> Answer {
        let config = self
            .home
            .harness_config(env)
            .map_err(|error| error.to_string())?;
        Ok(json!({
            "env": env,
            "path": self.home.harness_file(env),
            "profile": self.home.profile(env),
            "harness": config,
        }))
    }

    /// L3 change protocol: snapshot the overlay, merge the patch, ask the
    /// env's node to reload its profile. The running harness keeps the
    /// settings it started with; the next one reads the new file.
    pub fn config_patch(&mut self, env: &str, patch: &Value, reply: oneshot::Sender<Answer>) {
        if !patch.is_object() {
            let _ = reply.send(Err("config.patch needs a patch object".to_string()));
            return;
        }
        let outcome = self.home.patch_harness(env, patch);
        let (snapshot, config) = match outcome {
            Ok(pair) => pair,
            Err(error) => {
                let _ = reply.send(Err(error.to_string()));
                return;
            }
        };
        self.emit(
            "config.patch",
            Some(env),
            json!({"snapshot": snapshot, "keys": patch.as_object().map(|rows| rows.len())}),
        );
        let peer = self.nodes.get(env).and_then(|node| node.peer.clone());
        let timeout = Duration::from_millis(self.config.request_timeout_ms);
        let env = env.to_string();
        tokio::spawn(async move {
            let reload = match peer {
                Some(peer) => peer.request("reload", json!({}), timeout).await,
                None => Err("env is not registered".to_string()),
            };
            let _ = reply.send(Ok(json!({
                "ok": true,
                "env": env,
                "snapshot": snapshot,
                "harness": config,
                "reload": match reload {
                    Ok(result) => result,
                    Err(error) => json!({"ok": false, "error": error}),
                },
            })));
        });
    }

    /// P3.5 owns the queue; until then this is the honest stub: `auto` in the
    /// env's overlay approves everything, anything else denies with a reason
    /// the model can read and work around.
    pub fn approval_request(&mut self, env: &str, reason: &str) -> Answer {
        let mode = self
            .home
            .harness_config(env)
            .ok()
            .and_then(|config| {
                config
                    .get("approval")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "deny".to_string());
        let approved = mode == "auto";
        self.emit(
            "approval.request",
            Some(env),
            json!({"reason": reason, "approved": approved, "mode": mode}),
        );
        Ok(json!({
            "status": match approved {
                true => "approved",
                false => "denied",
            },
            "auto": approved,
            "reason": match approved {
                true => reason.to_string(),
                false => APPROVAL_OFF.to_string(),
            },
        }))
    }

    pub fn on_env_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::EventsAppend {
                env,
                kind,
                data,
                reply,
            } => {
                let _ = reply.send(self.events_append(&env, &kind, &data));
            }
            Cmd::EventsTail {
                env,
                after,
                limit,
                reply,
            } => {
                let _ = reply.send(self.events_tail(&env, after, limit));
            }
            Cmd::ConfigGet { env, reply } => {
                let _ = reply.send(self.config_get(&env));
            }
            Cmd::ConfigPatch { env, patch, reply } => self.config_patch(&env, &patch, reply),
            Cmd::Approval { env, reason, reply } => {
                let _ = reply.send(self.approval_request(&env, &reason));
            }
            Cmd::HarnessBoot { env } => self.harness_boot(&env),
            Cmd::HarnessReady { env, pid, error } => self.harness_ready(&env, pid, error),
            Cmd::HarnessExit {
                env,
                generation,
                code,
            } => self.harness_exit(&env, generation, code),
            _ => {}
        }
    }
}
