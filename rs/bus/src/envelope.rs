use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The five severities of RFC section 2, ordered so a subscriber's `levels`
/// filter can be a set membership rather than a threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    pub fn parse(text: &str) -> Level {
        match text {
            "trace" => Level::Trace,
            "debug" => Level::Debug,
            "warn" => Level::Warn,
            "error" => Level::Error,
            _ => Level::Info,
        }
    }
}

/// The one envelope of RFC section 2: a closed core that drives
/// storage/delivery/visibility and an open `tags`/`payload` that policy never
/// reads. Every event, log, metric and status frame from every language is one
/// of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub topic: String,
    #[serde(default)]
    pub ts: i64,
    #[serde(default)]
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default)]
    pub src: String,
    #[serde(default = "default_level")]
    pub level: Level,
    #[serde(default)]
    pub durable: bool,
    #[serde(default)]
    pub model_visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_s: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<i64>,
    #[serde(default)]
    pub event_id: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(default)]
    pub payload: Value,
}

fn default_level() -> Level {
    Level::Info
}

impl Envelope {
    /// A minimal envelope with a fresh `event_id` and `ts`; callers set the
    /// fields they care about. `model_visible` implies `durable`, which
    /// `normalize` enforces before publish.
    pub fn new(topic: impl Into<String>, level: Level, payload: Value) -> Self {
        Self {
            topic: topic.into(),
            ts: now_ms(),
            host: String::new(),
            env: None,
            src: String::new(),
            level,
            durable: false,
            model_visible: false,
            ttl_s: None,
            session: None,
            step: None,
            event_id: ulid(),
            tags: BTreeMap::new(),
            payload,
        }
    }

    /// RFC section 2: `model_visible` implies the session-log law, so it
    /// implies `durable`. Called once at the door of every publish.
    pub fn normalize(&mut self) {
        if self.model_visible {
            self.durable = true;
        }
        if self.event_id.is_empty() {
            self.event_id = ulid();
        }
        if self.ts == 0 {
            self.ts = now_ms();
        }
    }

    /// The latest-only compaction key: the `key` tag when present, else the
    /// topic. A status/metrics producer sets `key` to the thing whose newest
    /// value is all a subscriber wants.
    pub fn compaction_key(&self) -> &str {
        match self.tags.get("key") {
            Some(key) => key,
            None => &self.topic,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn from_value(value: Value) -> Result<Envelope, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A monotone-per-process ULID: 48-bit millisecond timestamp, then 16 bits of
/// per-process entropy and a 64-bit monotonic counter, Crockford base32.
/// Lexicographic order matches creation order within a process (same ms sorts
/// by the counter), which is what makes `event_id` a usable idempotency and
/// ordering key without pulling in a crate.
pub fn ulid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static BASE: AtomicU64 = AtomicU64::new(0);
    let ts = now_ms().max(0) as u64 & 0xFFFF_FFFF_FFFF;
    let mut base = BASE.load(Ordering::Relaxed);
    if base == 0 {
        base = (std::process::id() as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(now_ms() as u64)
            | 1;
        BASE.store(base, Ordering::Relaxed);
    }
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let rand_hi = (base >> 3) as u16;
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&ts.to_be_bytes()[2..8]);
    bytes[6..8].copy_from_slice(&rand_hi.to_be_bytes());
    bytes[8..16].copy_from_slice(&counter.to_be_bytes());
    encode_base32(&bytes)
}

fn encode_base32(bytes: &[u8; 16]) -> String {
    let mut value = 0u128;
    for byte in bytes {
        value = (value << 8) | *byte as u128;
    }
    let mut out = [0u8; 26];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(value & 0x1F) as usize];
        value >>= 5;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_preserves_every_field() {
        let mut env = Envelope::new("session/tool_result", Level::Info, json!({"n": 1}));
        env.env = Some("root".to_string());
        env.src = "harness".to_string();
        env.durable = true;
        env.session = Some("s1".to_string());
        env.step = Some(3);
        env.tags.insert("key".to_string(), "cpu".to_string());
        let bytes = env.encode();
        let back: Envelope = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(back.topic, env.topic);
        assert_eq!(back.env, env.env);
        assert_eq!(back.session, env.session);
        assert_eq!(back.step, env.step);
        assert_eq!(back.tags.get("key").map(String::as_str), Some("cpu"));
        assert_eq!(back.payload, env.payload);
        assert_eq!(back.event_id, env.event_id);
    }

    #[test]
    fn model_visible_implies_durable() {
        let mut env = Envelope::new("session/x", Level::Info, Value::Null);
        env.model_visible = true;
        env.normalize();
        assert!(env.durable);
    }

    #[test]
    fn ulids_are_26_chars_and_sort_by_time() {
        let a = ulid();
        let b = ulid();
        assert_eq!(a.len(), 26);
        assert_eq!(b.len(), 26);
        assert!(a < b, "{a} !< {b}");
    }

    #[test]
    fn compaction_key_prefers_the_key_tag() {
        let mut env = Envelope::new("metrics/cpu", Level::Info, Value::Null);
        assert_eq!(env.compaction_key(), "metrics/cpu");
        env.tags.insert("key".to_string(), "host7".to_string());
        assert_eq!(env.compaction_key(), "host7");
    }
}
