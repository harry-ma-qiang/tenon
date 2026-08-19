use crate::kv::KvFacade;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tenon_bus::{now_ms, Envelope, Hub, Level};
use tenon_storage::now;

const PREFIX: &str = "/timers/";
const MAX_SLEEP_MS: u64 = 60_000;

/// RFC section 9 P4.0's timer service: `timer.set{after_ms|every_ms, topic,
/// payload}` persists to the durable kv table under `/timers/` and one wheel in
/// base fires it as an envelope on schedule. Because the schedule is in kv it
/// survives a restart — the wheel reloads it on boot. cron is not parsed in
/// P4.0 (see rs/README.md); `after_ms` (one-shot) and `every_ms` (repeating)
/// are the two forms.
pub struct TimerService {
    kv: Arc<KvFacade>,
    hub: Arc<Hub>,
    seq: AtomicI64,
    wake: tokio::sync::Notify,
}

struct Timer {
    id: String,
    env: String,
    topic: String,
    payload: Value,
    every_ms: Option<i64>,
    fire_at: i64,
    ttl_s: Option<u32>,
}

impl TimerService {
    pub fn new(kv: Arc<KvFacade>, hub: Arc<Hub>) -> Arc<TimerService> {
        let service = Arc::new(TimerService {
            kv,
            hub,
            seq: AtomicI64::new(0),
            wake: tokio::sync::Notify::new(),
        });
        spawn_wheel(Arc::downgrade(&service));
        service
    }

    /// Register a timer. `after_ms` fires once, `every_ms` repeats; a caller may
    /// supply its own `id` (idempotent replace) or let one be minted.
    pub fn set(&self, env: &str, params: &Value) -> Result<Value, String> {
        let topic = params
            .get("topic")
            .and_then(Value::as_str)
            .ok_or("timer.set needs a topic")?
            .to_string();
        let after = params.get("after_ms").and_then(Value::as_i64);
        let every = params.get("every_ms").and_then(Value::as_i64);
        if params.get("cron").is_some() {
            return Err("cron is not supported in P4.0; use after_ms or every_ms".to_string());
        }
        let interval = after
            .or(every)
            .ok_or("timer.set needs after_ms or every_ms")?;
        if interval < 0 {
            return Err("timer interval must be >= 0".to_string());
        }
        // `timer_id`, not `id`: the wire frame's own `id` is the correlation key.
        let id = match params.get("timer_id").and_then(Value::as_str) {
            Some(id) => id.to_string(),
            None => format!(
                "t{}-{}",
                std::process::id(),
                self.seq.fetch_add(1, Ordering::Relaxed)
            ),
        };
        let record = json!({
            "id": id,
            "env": env,
            "topic": topic,
            "payload": params.get("payload").cloned().unwrap_or(Value::Null),
            "every_ms": every,
            "fire_at": now() + interval,
            "ttl_s": params.get("ttl_s").and_then(Value::as_u64),
        });
        self.kv.set(
            env,
            &format!("{PREFIX}{id}"),
            record.to_string().into_bytes(),
            true,
            None,
            None,
        )?;
        self.wake.notify_one();
        Ok(json!({"ok": true, "id": id, "fire_at": now() + interval}))
    }

    pub fn list(&self, env: &str) -> Value {
        let timers: Vec<Value> = self
            .kv
            .range(env, PREFIX)
            .into_iter()
            .filter_map(|(_, value, _)| serde_json::from_slice::<Value>(&value).ok())
            .collect();
        json!({"env": env, "count": timers.len(), "timers": timers})
    }

    pub fn del(&self, env: &str, id: &str) -> Value {
        let gone = self.kv.del(env, &format!("{PREFIX}{id}"));
        self.wake.notify_one();
        json!({"ok": gone, "id": id})
    }

    fn load(&self) -> Vec<Timer> {
        self.kv
            .scan_all(PREFIX)
            .into_iter()
            .filter_map(|(_, _, value)| serde_json::from_slice::<Value>(&value).ok())
            .filter_map(parse)
            .collect()
    }

    /// Fires every due timer, reschedules the repeating ones and deletes the
    /// one-shots, and returns how long to sleep until the next one.
    fn dispatch(&self, at: i64) -> Duration {
        let mut next = at + MAX_SLEEP_MS as i64;
        for timer in self.load() {
            if timer.fire_at > at {
                next = next.min(timer.fire_at);
                continue;
            }
            self.fire(&timer);
            match timer.every_ms {
                Some(every) if every > 0 => {
                    let fire_at = at + every;
                    self.reschedule(&timer, fire_at);
                    next = next.min(fire_at);
                }
                _ => {
                    self.del(&timer.env, &timer.id);
                }
            }
        }
        Duration::from_millis((next - at).clamp(1, MAX_SLEEP_MS as i64) as u64)
    }

    fn fire(&self, timer: &Timer) {
        let mut envelope = Envelope::new(timer.topic.clone(), Level::Info, timer.payload.clone());
        envelope.env = Some(timer.env.clone());
        envelope.src = "timer".to_string();
        envelope.durable = true;
        envelope.ttl_s = timer.ttl_s;
        envelope.ts = now_ms();
        self.hub.emit(envelope);
    }

    fn reschedule(&self, timer: &Timer, fire_at: i64) {
        let record = json!({
            "id": timer.id,
            "env": timer.env,
            "topic": timer.topic,
            "payload": timer.payload,
            "every_ms": timer.every_ms,
            "fire_at": fire_at,
            "ttl_s": timer.ttl_s,
        });
        let _ = self.kv.set(
            &timer.env,
            &format!("{PREFIX}{}", timer.id),
            record.to_string().into_bytes(),
            true,
            None,
            None,
        );
    }
}

fn spawn_wheel(service: Weak<TimerService>) {
    tokio::spawn(async move {
        loop {
            let Some(service) = service.upgrade() else {
                return;
            };
            let sleep = service.dispatch(now());
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = service.wake.notified() => {}
            }
        }
    });
}

fn parse(value: Value) -> Option<Timer> {
    Some(Timer {
        id: value.get("id")?.as_str()?.to_string(),
        env: value.get("env")?.as_str()?.to_string(),
        topic: value.get("topic")?.as_str()?.to_string(),
        payload: value.get("payload").cloned().unwrap_or(Value::Null),
        every_ms: value.get("every_ms").and_then(Value::as_i64),
        fire_at: value.get("fire_at").and_then(Value::as_i64).unwrap_or(0),
        ttl_s: value.get("ttl_s").and_then(Value::as_u64).map(|s| s as u32),
    })
}
