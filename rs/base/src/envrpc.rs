use crate::base::Base;
use crate::params::{i64_or, parse, text};
use crate::rpc::Cmd;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tenon_storage::{Retention, Store};
use tokio::sync::oneshot;

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
        let answer = json!({"id": event.id, "at": event.at});
        self.account(env, kind, data);
        self.publish_event(kind, Some(env), data);
        Ok(answer)
    }

    /// An env-scoped fact belongs in that env's own log, which is what
    /// `log.query{env}`, `tenon run` and the UI's event tail read. Before
    /// the env has a state file it falls back to the barebone's log.
    pub fn emit_env(&mut self, env: &str, kind: &str, data: Value) {
        let has_store = self
            .nodes
            .get(env)
            .map(|node| node.store.is_some())
            .unwrap_or(false);
        match has_store {
            true => {
                let _ = self.events_append(env, kind, &data);
            }
            false => self.emit(kind, Some(env), data),
        }
    }

    /// The internal window reader behind `log.query`: the newest events of one
    /// env's log after `after`. `env: "base"` reads the barebone's own log
    /// instead of an env's — boot, LKG, probe and sandbox facts are base-wide
    /// and belong to no env. No longer its own RPC; `log.query` is the surface.
    pub fn events_tail(&self, env: &str, after: i64, limit: i64) -> Answer {
        let store = match env {
            "base" => &self.store,
            _ => {
                let Some(node) = self.nodes.get(env) else {
                    return Err(format!("unknown env {env}"));
                };
                let Some(store) = node.store.as_ref() else {
                    return Err(format!("env {env} has no state file"));
                };
                store
            }
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

    /// `log.query{env, session?, after?, limit?}`: the typed session-log reader
    /// (RFC section 3). It is the log window plus an optional `session`
    /// narrowing, folded in one place so `session.history`, the UI and the CLI
    /// share one path into the env's log without any of them touching sqlite or
    /// a topic glob.
    pub fn log_query(&self, env: &str, after: i64, limit: i64, session: Option<&str>) -> Answer {
        let tail = self.events_tail(env, after, limit)?;
        let Some(session) = session else {
            return Ok(tail);
        };
        let events: Vec<Value> = tail
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|event| crate::params::str_of(&event["data"], "session") == Some(session))
            .collect();
        Ok(json!({"env": env, "count": events.len(), "session": session, "events": events}))
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
    /// `target: "base"` patches the barebone's own `config.yml` instead, which
    /// is L0 and always behind a human gate.
    pub fn config_patch(
        &mut self,
        env: &str,
        target: &str,
        patch: &Value,
        approved: bool,
        reply: oneshot::Sender<Answer>,
    ) {
        if !patch.is_object() {
            let _ = reply.send(Err("config.patch needs a patch object".to_string()));
            return;
        }
        let base_config = target == "base";
        if !approved && (base_config || self.config.approval.gate_config_patch) {
            let reason = format!("config.patch of the {target} config from {env}: {patch}");
            let (env, target, patch) = (env.to_string(), target.to_string(), patch.clone());
            let name = env.clone();
            self.gate(&name, "config.patch", &reason, reply, move |reply| {
                Cmd::ConfigPatch {
                    env,
                    target,
                    patch,
                    approved: true,
                    reply,
                }
            });
            return;
        }
        if base_config {
            let _ = reply.send(self.patch_base_config(patch));
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

    /// L0 in RFC section 10's table: the barebone's own config, snapshotted
    /// before every change and read at the next `tenon start`. Base never
    /// reloads it into a running process.
    fn patch_base_config(&mut self, patch: &Value) -> Answer {
        let path = self.home.config_file();
        let dir = self.home.config_snapshots("base");
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let stamp = tenon_storage::now();
        let snapshot = dir.join(format!("config-{stamp}.yml"));
        std::fs::copy(&path, &snapshot).map_err(|error| error.to_string())?;
        let body = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let mut config: Value = serde_yaml::from_str(&body).map_err(|error| error.to_string())?;
        crate::home::merge(&mut config, patch);
        let body = serde_yaml::to_string(&config).map_err(|error| error.to_string())?;
        std::fs::write(&path, body).map_err(|error| error.to_string())?;
        self.emit(
            "config.patch",
            None,
            json!({"target": "base", "snapshot": snapshot}),
        );
        Ok(json!({
            "ok": true,
            "target": "base",
            "snapshot": snapshot,
            "config": config,
            "applies": "next start",
        }))
    }

    pub(crate) fn store_of(&self, env: &str) -> Result<&Store, String> {
        let Some(node) = self.nodes.get(env) else {
            return Err(format!("unknown env {env}"));
        };
        node.store
            .as_ref()
            .ok_or_else(|| format!("env {env} has no state file"))
    }

    /// Everything the harness records beside the log. The harness never opens
    /// sqlite: it sends one of these frames and base, the file's only writer,
    /// performs it.
    pub fn records(&mut self, env: &str, method: &str, params: &Value) -> Answer {
        match method {
            "episodes.append" => self.episodes_append(env, params),
            "tool_results.append" => self.tool_result_append(env, params),
            "blobs.put" => self.blobs_put(env, params),
            "blobs.get" => self.blobs_get(env, params),
            "state.retain" => self.state_retain(env),
            other => Err(format!("unknown_method:{other}")),
        }
    }

    /// One row per step of that env's loop. `state_hash` is computed here
    /// rather than in the harness because base is what holds the workspace
    /// history: the newest snapshot ref and the id of the user message the
    /// step is answering are what identify the state the step started from.
    fn episodes_append(&mut self, env: &str, params: &Value) -> Answer {
        let store = self.store_of(env)?;
        let row: Episode = parse(params)?;
        if row.session_id.is_empty() {
            return Err("episodes.append needs a session_id".to_string());
        }
        let hash = match row.state_hash {
            Some(given) => given,
            None => {
                let head = store
                    .head_snapshot()
                    .ok()
                    .flatten()
                    .map(|row| row.reference)
                    .unwrap_or_default();
                state_hash(&head, row.user_event)
            }
        };
        let id = store
            .put_episode(
                &row.session_id,
                row.step,
                &hash,
                &row.action,
                row.verifier_score,
                &row.cost,
            )
            .map_err(|error| error.to_string())?;
        Ok(json!({"id": id, "state_hash": hash, "step": row.step}))
    }

    fn tool_result_append(&mut self, env: &str, params: &Value) -> Answer {
        let store = self.store_of(env)?;
        let row: ToolResult = parse(params)?;
        let status = match row.status.is_empty() {
            true => "ok".to_string(),
            false => row.status,
        };
        let id = store
            .put_tool_result(
                row.event_id,
                &row.name,
                &status,
                row.duration_ms,
                row.blob_hash.as_deref(),
            )
            .map_err(|error| error.to_string())?;
        Ok(json!({"id": id}))
    }

    fn blobs_put(&mut self, env: &str, params: &Value) -> Answer {
        let data = text(params, "data");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.as_bytes())
            .map_err(|error| format!("blobs.put needs base64 data: {error}"))?;
        let hash = self
            .store_of(env)?
            .put_blob(&bytes)
            .map_err(|error| error.to_string())?;
        Ok(json!({"hash": hash, "size": bytes.len()}))
    }

    /// The whole blob, or the window `{offset, len}` names — the incremental
    /// read that keeps a 100 MB tool output pageable rather than loaded.
    fn blobs_get(&self, env: &str, params: &Value) -> Answer {
        let store = self.store_of(env)?;
        let hash = text(params, "hash");
        let Some(row) = store.blob(&hash).map_err(|error| error.to_string())? else {
            return Err(format!("unknown blob {hash}"));
        };
        let offset = i64_or(params, "offset", 0);
        let len = params.get("len").and_then(Value::as_i64);
        let bytes = match len {
            Some(len) => store
                .open_blob(&hash, offset, len)
                .map_err(|error| error.to_string())?,
            None => store
                .get_blob(&hash)
                .map_err(|error| error.to_string())?
                .unwrap_or_default(),
        };
        Ok(json!({
            "hash": hash,
            "size": row.size,
            "offset": offset,
            "len": bytes.len(),
            "created_at": row.created_at,
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        }))
    }

    /// The retention policy from `config.yml`, run against one env's state
    /// file. The newest snapshot ref is passed in as an LKG ref: it is what a
    /// reset would restore, so it survives whatever the window says.
    fn state_retain(&mut self, env: &str) -> Answer {
        let config = self.config.retention.clone();
        let store = self.store_of(env)?;
        let head = store
            .head_snapshot()
            .ok()
            .flatten()
            .map(|row| row.reference)
            .unwrap_or_default();
        let policy = Retention {
            keep_steps: config.keep_steps,
            milestone_every: config.milestone_every,
            keep_refs: match head.is_empty() {
                true => vec![],
                false => vec![head],
            },
            keep_events: config.keep_events,
            blob_grace_ms: config.blob_grace_ms,
        };
        let out = store.retain(&policy).map_err(|error| error.to_string())?;
        let left = json!({
            "packs": store.pack_count().unwrap_or(0),
            "blobs": store.blob_count().unwrap_or(0),
            "events": store.event_count().unwrap_or(0),
            "episodes": store.episode_count().unwrap_or(0),
        });
        let removed = serde_json::to_value(&out).unwrap_or(Value::Null);
        self.emit(
            "state.retain",
            Some(env),
            json!({"removed": removed, "left": left}),
        );
        Ok(json!({"ok": true, "env": env, "removed": removed, "left": left}))
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
            Cmd::Query {
                env,
                method,
                params,
                reply,
            } => {
                let _ = reply.send(self.query(&env, &method, &params));
            }
            Cmd::LogQuery {
                env,
                after,
                limit,
                session,
                reply,
            } => {
                let _ = reply.send(self.log_query(&env, after, limit, session.as_deref()));
            }
            Cmd::Records {
                env,
                method,
                params,
                reply,
            } => {
                let _ = reply.send(self.records(&env, &method, &params));
            }
            Cmd::ConfigGet { env, reply } => {
                let _ = reply.send(self.config_get(&env));
            }
            Cmd::ConfigPatch {
                env,
                target,
                patch,
                approved,
                reply,
            } => self.config_patch(&env, &target, &patch, approved, reply),
            Cmd::Approval {
                env,
                reason,
                kind,
                reply,
            } => self.approval_request(&env, &reason, &kind, reply),
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

fn empty_object() -> Value {
    json!({})
}

/// One step of an env's loop, as `episodes.append` carries it.
#[derive(Deserialize)]
struct Episode {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    step: i64,
    #[serde(default)]
    action: Value,
    #[serde(default = "empty_object")]
    cost: Value,
    #[serde(default)]
    verifier_score: Option<f64>,
    #[serde(default)]
    user_event: i64,
    #[serde(default)]
    state_hash: Option<String>,
}

#[derive(Deserialize)]
struct ToolResult {
    #[serde(default)]
    event_id: i64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    duration_ms: i64,
    #[serde(default)]
    blob_hash: Option<String>,
}

/// The state a step started from, in 16 hex chars: the workspace at that
/// moment (the newest snapshot ref) plus the message being answered.
fn state_hash(reference: &str, user_event: i64) -> String {
    crate::hash::short(format!("{reference}:{user_event}"), 8)
}
