use crate::client::Client;
use crate::kv::KvFacade;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tenon_bus::{glob, now_ms, Envelope, Filter, Hub, Level, SubOpts};
use tenon_storage::now;

const PREFIX: &str = "/triggers/";

/// The tuning a trigger obeys (RFC 8d.3): the hop cap that stops a
/// publish -> trigger -> publish loop, the per-trigger outbound-call rate, the
/// retry count for `http_post`, and which action kinds require a human.
#[derive(Debug, Clone)]
pub struct TriggerConfig {
    pub hop_cap: u32,
    pub calls_per_min: u32,
    pub http_retries: u32,
    pub gated_actions: Vec<String>,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            hop_cap: 4,
            calls_per_min: 60,
            http_retries: 3,
            gated_actions: Vec::new(),
        }
    }
}

struct Trigger {
    id: String,
    env: String,
    admin: bool,
    topics: Vec<String>,
    tags: HashMap<String, String>,
    action: Value,
}

/// RFC P4.7 triggers: a durable kv-stored rule (`/triggers/<id>` per env) that
/// fires an action when a bus envelope of its own env matches its filter. One
/// hub subscription in base watches every envelope; each action runs off the
/// bus on its own task so the fabric never blocks. Rules survive a restart
/// because they live in the durable kv table, reloaded on boot.
pub struct TriggerService {
    kv: Arc<KvFacade>,
    hub: Arc<Hub>,
    sock: PathBuf,
    config: TriggerConfig,
    seq: AtomicI64,
    rules: Mutex<Vec<Arc<Trigger>>>,
    budgets: Mutex<HashMap<String, (i64, u32)>>,
    http: reqwest::Client,
}

impl TriggerService {
    pub fn new(
        kv: Arc<KvFacade>,
        hub: Arc<Hub>,
        sock: PathBuf,
        config: TriggerConfig,
    ) -> Arc<TriggerService> {
        let service = Arc::new(TriggerService {
            kv,
            hub,
            sock,
            config,
            seq: AtomicI64::new(0),
            rules: Mutex::new(Vec::new()),
            budgets: Mutex::new(HashMap::new()),
            http: reqwest::Client::new(),
        });
        service.reload();
        spawn_loop(Arc::downgrade(&service));
        service
    }

    /// `trigger.set{trigger_id?, filter{topics, tags?}, action, ttl?}`. `admin`
    /// records whether the caller was unscoped (base/barebone): only an admin
    /// trigger may target another env with a `prompt` action. The id travels as
    /// `trigger_id` because the frame's own `id` is the wire correlation key.
    pub fn set(&self, env: &str, admin: bool, body: &Value) -> Result<Value, String> {
        let filter = body.get("filter").cloned().unwrap_or(json!({}));
        let action = body.get("action").cloned().unwrap_or(Value::Null);
        if action.get("type").and_then(Value::as_str).is_none() {
            return Err("trigger.set needs action.type".to_string());
        }
        let id = match body.get("trigger_id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => format!(
                "tg{}-{}",
                std::process::id(),
                self.seq.fetch_add(1, Ordering::Relaxed)
            ),
        };
        let ttl_ms = body
            .get("ttl")
            .or_else(|| body.get("ttl_s"))
            .and_then(Value::as_i64)
            .map(|ttl| ttl * 1000);
        let record = json!({
            "id": id,
            "env": env,
            "admin": admin,
            "filter": filter,
            "action": action,
            "created_at": now(),
        });
        self.kv.set(
            env,
            &format!("{PREFIX}{id}"),
            record.to_string().into_bytes(),
            true,
            ttl_ms,
            None,
        )?;
        self.reload();
        Ok(json!({"ok": true, "id": id}))
    }

    pub fn list(&self, env: &str) -> Value {
        let rows: Vec<Value> = self
            .kv
            .range(env, PREFIX)
            .into_iter()
            .filter_map(|(_, value, _)| serde_json::from_slice::<Value>(&value).ok())
            .collect();
        json!({"env": env, "count": rows.len(), "triggers": rows})
    }

    pub fn del(&self, env: &str, id: &str) -> Value {
        let gone = self.kv.del(env, &format!("{PREFIX}{id}"));
        self.reload();
        json!({"ok": gone, "id": id})
    }

    fn reload(&self) {
        let rules: Vec<Arc<Trigger>> = self
            .kv
            .scan_all(PREFIX)
            .into_iter()
            .filter_map(|(env, _, value)| parse(&env, &value))
            .map(Arc::new)
            .collect();
        *self.rules.lock().expect("triggers") = rules;
    }

    fn matching(&self, envelope: &Envelope) -> Vec<Arc<Trigger>> {
        let env = envelope.env.as_deref();
        self.rules
            .lock()
            .expect("triggers")
            .iter()
            .filter(|rule| Some(rule.env.as_str()) == env && rule.matches(envelope))
            .cloned()
            .collect()
    }

    fn on_envelope(self: &Arc<Self>, envelope: &Envelope) {
        if envelope.src == "trigger" && envelope.hop == 0 {
            return;
        }
        for rule in self.matching(envelope) {
            let service = self.clone();
            let source = envelope.clone();
            tokio::spawn(async move { service.fire(rule, source).await });
        }
    }

    async fn fire(self: Arc<Self>, rule: Arc<Trigger>, source: Envelope) {
        let kind = rule
            .action
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if self.config.gated_actions.iter().any(|name| name == &kind)
            && !self.approved(&rule, &kind).await
        {
            return;
        }
        match kind.as_str() {
            "publish" => self.act_publish(&rule, &source).await,
            "http_post" => self.act_http_post(&rule, &source).await,
            "prompt" => self.act_prompt(&rule, &source).await,
            _ => {}
        }
    }

    async fn act_publish(&self, rule: &Trigger, source: &Envelope) {
        let hop = source.hop + 1;
        if hop > self.config.hop_cap {
            return;
        }
        let topic = match rule.action.get("topic").and_then(Value::as_str) {
            Some(topic) => topic.to_string(),
            None => return,
        };
        let template = rule
            .action
            .get("payload_template")
            .cloned()
            .unwrap_or(Value::Null);
        let mut envelope = Envelope::new(topic, Level::Info, render(&template, source));
        envelope.env = Some(rule.env.clone());
        envelope.src = "trigger".to_string();
        envelope.hop = hop;
        let _ = self.hub.publish(envelope).await;
    }

    async fn act_http_post(&self, rule: &Trigger, source: &Envelope) {
        if !self.spend(&rule.id) {
            return;
        }
        let url = match rule.action.get("url").and_then(Value::as_str) {
            Some(url) => url.to_string(),
            None => return,
        };
        let headers = rule.action.get("headers").cloned().unwrap_or(Value::Null);
        let payload = source.to_value();
        let tries = self.config.http_retries.max(1);
        for attempt in 0..tries {
            let mut request = self.http.post(&url).json(&payload);
            if let Some(map) = headers.as_object() {
                for (name, value) in map {
                    if let Some(value) = value.as_str() {
                        request = request.header(name, value);
                    }
                }
            }
            match request.send().await {
                Ok(response) if response.status().is_success() => return,
                _ => {
                    let backoff = 100u64 * (1 << attempt.min(5));
                    tokio::time::sleep(Duration::from_millis(backoff)).await;
                }
            }
        }
    }

    async fn act_prompt(&self, rule: &Trigger, source: &Envelope) {
        let target = match rule.action.get("env").and_then(Value::as_str) {
            // A cross-env prompt is a barebone-only capability (RFC 8d.3): a
            // scoped-created trigger is confined to its own env.
            Some(env) if env != rule.env => match rule.admin {
                true => env.to_string(),
                false => rule.env.clone(),
            },
            _ => rule.env.clone(),
        };
        let template = rule
            .action
            .get("text_template")
            .cloned()
            .unwrap_or(json!(""));
        let text = match render(&template, source) {
            Value::String(text) => text,
            other => other.to_string(),
        };
        if text.trim().is_empty() {
            return;
        }
        let _ = self.prompt(&target, &text).await;
    }

    async fn prompt(&self, env: &str, text: &str) -> anyhow::Result<()> {
        let mut client = Client::connect(&self.sock).await?;
        let created = client.call("session.create", json!({"env": env})).await?;
        let session = created
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("no session_id"))?
            .to_string();
        client
            .call(
                "session.prompt",
                json!({"env": env, "session_id": session, "text": text}),
            )
            .await?;
        Ok(())
    }

    async fn approved(&self, rule: &Trigger, kind: &str) -> bool {
        let Ok(mut client) = Client::connect(&self.sock).await else {
            return false;
        };
        let answer = client
            .call(
                "approval.request",
                json!({
                    "env": rule.env,
                    "reason": format!("trigger {} action {kind}", rule.id),
                    "kind": "trigger",
                }),
            )
            .await;
        matches!(answer, Ok(value) if value.get("status").and_then(Value::as_str) == Some("approved"))
    }

    /// One outbound `http_post` budget slot: a fixed 60 s window per trigger id,
    /// `calls_per_min` allowed inside it.
    fn spend(&self, id: &str) -> bool {
        if self.config.calls_per_min == 0 {
            return true;
        }
        let now = now_ms();
        let mut budgets = self.budgets.lock().expect("budgets");
        let entry = budgets.entry(id.to_string()).or_insert((now, 0));
        if now - entry.0 >= 60_000 {
            *entry = (now, 0);
        }
        if entry.1 >= self.config.calls_per_min {
            return false;
        }
        entry.1 += 1;
        true
    }
}

impl Trigger {
    fn matches(&self, envelope: &Envelope) -> bool {
        if !self.topics.is_empty()
            && !self
                .topics
                .iter()
                .any(|pattern| glob(pattern, &envelope.topic))
        {
            return false;
        }
        for (key, value) in &self.tags {
            if envelope.tags.get(key).map(String::as_str) != Some(value.as_str()) {
                return false;
            }
        }
        true
    }
}

fn spawn_loop(service: Weak<TriggerService>) {
    let Some(strong) = service.upgrade() else {
        return;
    };
    let subscription = strong.hub.subscribe(Filter::all(), SubOpts::default());
    drop(strong);
    tokio::spawn(async move {
        loop {
            let Some(batch) = subscription.recv().await else {
                return;
            };
            let Some(service) = service.upgrade() else {
                return;
            };
            for message in batch {
                service.on_envelope(&message.envelope);
            }
        }
    });
}

fn parse(env: &str, value: &[u8]) -> Option<Trigger> {
    let record: Value = serde_json::from_slice(value).ok()?;
    let filter = record.get("filter").cloned().unwrap_or(Value::Null);
    let topics = filter
        .get("topics")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let tags = filter
        .get("tags")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Some(Trigger {
        id: record.get("id")?.as_str()?.to_string(),
        env: record
            .get("env")
            .and_then(Value::as_str)
            .unwrap_or(env)
            .to_string(),
        admin: record
            .get("admin")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        topics,
        tags,
        action: record.get("action").cloned().unwrap_or(Value::Null),
    })
}

/// Minimal templating (RFC P4.7 "keep minimal"): a string that is exactly
/// `${payload}` becomes the whole source payload; otherwise `${payload.<key>}`,
/// `${topic}`, `${env}` and `${session}` are substituted as text inside any
/// string, and objects/arrays are walked. A `null` template copies the source
/// payload verbatim.
fn render(template: &Value, source: &Envelope) -> Value {
    match template {
        Value::Null => source.payload.clone(),
        Value::String(text) if text == "${payload}" => source.payload.clone(),
        Value::String(text) => Value::String(substitute(text, source)),
        Value::Array(items) => Value::Array(items.iter().map(|it| render(it, source)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), render(value, source)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn substitute(text: &str, source: &Envelope) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        out.push_str(&resolve(&after[..end], source));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn resolve(key: &str, source: &Envelope) -> String {
    match key {
        "topic" => source.topic.clone(),
        "env" => source.env.clone().unwrap_or_default(),
        "session" => source.session.clone().unwrap_or_default(),
        other => match other.strip_prefix("payload.") {
            Some(field) => match source.payload.get(field) {
                Some(Value::String(text)) => text.clone(),
                Some(value) => value.to_string(),
                None => String::new(),
            },
            None => String::new(),
        },
    }
}
