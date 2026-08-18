use crate::bus::{BoxFut, Event, Log};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tenon_base::client::Client;
use tokio::sync::Mutex;

/// Nothing base answers should take this long; a call that does is a wedge,
/// and the lane has to come back rather than hold every later call behind it.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Two connections, not one: the session log writes constantly and must never
/// queue behind a `plugin.mount` that is waiting for a plugin's handshake.
pub const LOG: usize = 0;
pub const SLOW: usize = 1;

/// One connection to base's front door, re-dialled on failure. Every host-side
/// thing the harness needs — the session log, the config overlay, spawning a
/// child runtime, asking for an approval — is a frame on this socket.
pub struct Api {
    sock: PathBuf,
    env: String,
    lanes: [Mutex<Option<Client>>; 2],
}

impl Api {
    pub fn new(sock: PathBuf, env: String) -> Self {
        Self {
            sock,
            env,
            lanes: [Mutex::new(None), Mutex::new(None)],
        }
    }

    pub fn env(&self) -> &str {
        &self.env
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.lane(SLOW, method, params).await
    }

    pub async fn lane(&self, lane: usize, method: &str, params: Value) -> Result<Value, String> {
        match tokio::time::timeout(CALL_TIMEOUT, self.talk(lane, method, params)).await {
            Ok(answer) => answer,
            Err(_) => {
                *self.lanes[lane].lock().await = None;
                Err(format!("base {method}: timed out"))
            }
        }
    }

    async fn talk(&self, lane: usize, method: &str, params: Value) -> Result<Value, String> {
        let mut slot = self.lanes[lane].lock().await;
        for attempt in 0..2 {
            if slot.is_none() {
                match Client::connect(&self.sock).await {
                    Ok(client) => *slot = Some(client),
                    Err(error) => return Err(format!("base: {error}")),
                }
            }
            let Some(client) = slot.as_mut() else {
                continue;
            };
            match client.call(method, params.clone()).await {
                Ok(result) => return Ok(result),
                Err(error) => {
                    *slot = None;
                    if attempt == 1 {
                        return Err(format!("base {method}: {error}"));
                    }
                }
            }
        }
        Err(format!("base {method}: unreachable"))
    }

    pub async fn env_call(&self, method: &str, params: Value) -> Result<Value, String> {
        self.env_lane(SLOW, method, params).await
    }

    pub async fn env_lane(
        &self,
        lane: usize,
        method: &str,
        mut params: Value,
    ) -> Result<Value, String> {
        if let Some(object) = params.as_object_mut() {
            object.insert("env".to_string(), json!(self.env));
        }
        self.lane(lane, method, params).await
    }
}

pub struct BaseLog {
    api: std::sync::Arc<Api>,
}

impl BaseLog {
    pub fn new(api: std::sync::Arc<Api>) -> Self {
        Self { api }
    }
}

impl Log for BaseLog {
    fn append<'a>(&'a self, kind: &str, data: Value) -> BoxFut<'a, Result<i64, String>> {
        let kind = kind.to_string();
        Box::pin(async move {
            let answer = self
                .api
                .env_lane(
                    crate::api::LOG,
                    "events.append",
                    json!({"kind": kind, "data": data}),
                )
                .await?;
            Ok(answer.get("id").and_then(Value::as_i64).unwrap_or(0))
        })
    }

    fn tail<'a>(&'a self, after: i64, limit: i64) -> BoxFut<'a, Result<Vec<Event>, String>> {
        Box::pin(async move {
            let answer = self
                .api
                .env_lane(
                    crate::api::LOG,
                    "events.tail",
                    json!({"after": after, "limit": limit}),
                )
                .await?;
            Ok(rows(&answer))
        })
    }
}

pub fn rows(answer: &Value) -> Vec<Event> {
    answer
        .get("events")
        .and_then(Value::as_array)
        .map(|events| {
            events
                .iter()
                .map(|row| Event {
                    id: row.get("id").and_then(Value::as_i64).unwrap_or(0),
                    at: row.get("at").and_then(Value::as_i64).unwrap_or(0),
                    kind: row
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    data: row.get("data").cloned().unwrap_or(Value::Null),
                })
                .collect()
        })
        .unwrap_or_default()
}
