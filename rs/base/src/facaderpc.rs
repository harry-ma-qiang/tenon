use crate::bus::Facades;
use crate::params::{i64_or, str_of, text_or};
use crate::peer::Peer;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tenon_bus::{Envelope, Filter, Hub, Level, Published, SubOpts, Subscription};
use tokio::sync::{mpsc, oneshot, watch};

type Answer = Result<Value, String>;
type Cmds = mpsc::UnboundedSender<Cmd>;

/// Everything the front door knows about one open connection that the facades
/// need: the peer to stream envelopes to, the env this connection is scoped to
/// (RFC 8d.2 — `None` means an unscoped base/CLI caller), and a cancel that
/// fires when the client disconnects so subscribe pumps stop.
#[derive(Clone)]
pub struct Conn {
    pub peer: Peer,
    pub scope: Arc<Mutex<Option<String>>>,
    pub cancel: watch::Receiver<bool>,
    pub root_env: String,
}

impl Conn {
    pub fn new(peer: Peer, root_env: String, cancel: watch::Receiver<bool>) -> Conn {
        Conn {
            peer,
            scope: Arc::new(Mutex::new(None)),
            cancel,
            root_env,
        }
    }

    fn bound(&self) -> Option<String> {
        self.scope.lock().expect("scope").clone()
    }

    /// The optional-env resolution: a scoped caller can never name another env;
    /// an unscoped caller keeps whatever it named (`None` = every env, for a
    /// firehose subscribe).
    fn scoped_opt(&self, requested: Option<&str>) -> Result<Option<String>, String> {
        match self.bound() {
            Some(env) => match requested {
                Some(other) if other != env => Err("cross_env_denied".to_string()),
                _ => Ok(Some(env)),
            },
            None => Ok(requested.map(str::to_string)),
        }
    }

    /// The concrete-env resolution for kv/blob/timer, which always act inside one
    /// env: the bound env for a scoped caller, else the named or root env.
    fn scoped_env(&self, requested: Option<&str>) -> Result<String, String> {
        Ok(self
            .scoped_opt(requested)?
            .unwrap_or_else(|| self.root_env.clone()))
    }
}

/// `auth.scope{env, token}`: binds this connection to one env after checking the
/// env's runtime token with the actor. Every later facade call is then pinned to
/// that env. base/CLI callers never call this and stay unscoped.
pub async fn scope(conn: &Conn, body: &Value, cmds: &Cmds) -> Answer {
    let env = text_or(body, "env", "");
    let token = text_or(body, "token", "");
    let (tx, rx) = oneshot::channel();
    cmds.send(Cmd::ScopeCheck {
        env: env.clone(),
        token,
        reply: tx,
    })
    .map_err(|_| "base_gone".to_string())?;
    if rx.await.map_err(|_| "base_gone".to_string())? {
        *conn.scope.lock().expect("scope") = Some(env.clone());
        Ok(json!({"ok": true, "env": env}))
    } else {
        Err("unauthorized".to_string())
    }
}

fn req_env(body: &Value) -> Option<String> {
    str_of(body, "env").map(str::to_string)
}

pub async fn bus_publish(conn: &Conn, facades: &Facades, body: &Value) -> Answer {
    let mut envelope = match body.get("envelope").cloned() {
        Some(value) => Envelope::from_value(value)?,
        None => Envelope::from_value(body.clone())?,
    };
    if let Some(env) = conn.scoped_opt(envelope.env.as_deref())? {
        envelope.env = Some(env);
    }
    let offset = facades.hub.publish(envelope).await?;
    Ok(json!({"ok": true, "offset": offset}))
}

/// `bus.subscribe`: register on the hub and stream every matching envelope to
/// the peer as a `t:"ev"` frame. Returns at once; a background pump runs until
/// the client disconnects (its cancel fires) or the ring closes.
pub fn bus_subscribe(conn: &Conn, facades: &Facades, body: &Value) -> Answer {
    let scoped = conn.scoped_opt(req_env(body).as_deref())?;
    let filter = filter_from(body, scoped, conn.bound().is_some());
    let opts = SubOpts {
        since_offset: body.get("since_offset").and_then(Value::as_i64),
        coalesce_ms: body.get("coalesce_ms").and_then(Value::as_u64),
        latest_only: body
            .get("latest_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        capacity: body
            .get("capacity")
            .and_then(Value::as_u64)
            .map(|n| n as usize),
    };
    let subscription = facades.hub.subscribe(filter, opts);
    let offset = facades.hub.max_offset();
    pump(conn.clone(), subscription);
    Ok(json!({"ok": true, "offset": offset}))
}

fn filter_from(body: &Value, env: Option<String>, scoped: bool) -> Filter {
    let topics = body
        .get("topics")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let levels = body
        .get("levels")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(Level::parse)
                .collect()
        })
        .unwrap_or_default();
    Filter {
        topics,
        levels,
        env,
        session: str_of(body, "session").map(str::to_string),
        scoped,
        ..Filter::default()
    }
}

fn pump(conn: Conn, subscription: Subscription) {
    tokio::spawn(async move {
        let mut cancel = conn.cancel.clone();
        loop {
            if *cancel.borrow() {
                return;
            }
            tokio::select! {
                _ = cancel.changed() => return,
                batch = subscription.recv() => match batch {
                    Some(batch) => {
                        for msg in batch {
                            conn.peer.send(ev_frame(&msg));
                        }
                    }
                    None => return,
                },
            }
        }
    });
}

fn ev_frame(msg: &Published) -> Value {
    let mut frame = msg.envelope.to_value();
    if let Some(object) = frame.as_object_mut() {
        object.insert("t".to_string(), json!("ev"));
        object.insert("offset".to_string(), json!(msg.offset));
    }
    frame
}

/// `kv.*`: every RFC section 3 kv verb, env-scoped. `kv.watch` is the streaming
/// one — a since_rev snapshot then a live `kv/<key>` subscription.
pub fn kv(conn: &Conn, facades: &Facades, method: &str, body: &Value) -> Answer {
    let env = conn.scoped_env(req_env(body).as_deref())?;
    let kv = &facades.kv;
    let key = || text_or(body, "key", "");
    let durable = body
        .get("durable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match method {
        "kv.get" => match kv.get(&env, &key()) {
            Some((value, rev)) => Ok(json!({"found": true, "value": as_text(&value), "rev": rev})),
            None => Ok(json!({"found": false})),
        },
        "kv.set" => {
            let ttl = body.get("ttl_ms").and_then(Value::as_i64);
            let lease = str_of(body, "lease_id").map(str::to_string);
            let rev = kv.set(&env, &key(), value_of(body), durable, ttl, lease)?;
            Ok(json!({"ok": true, "rev": rev, "revision": kv.revision()}))
        }
        "kv.del" => Ok(json!({"ok": kv.del(&env, &key())})),
        "kv.cas" => {
            let expect = body.get("expect").and_then(Value::as_str).map(as_bytes);
            let rev = kv.cas(&env, &key(), expect, value_of(body), durable)?;
            Ok(json!({"ok": true, "rev": rev}))
        }
        "kv.incr" => {
            let delta = i64_or(body, "delta", 1);
            Ok(json!({"value": kv.incr(&env, &key(), delta, durable)?}))
        }
        "kv.expire" => {
            let ttl = i64_or(body, "ttl_ms", 0);
            Ok(json!({"rev": kv.expire(&env, &key(), ttl)?}))
        }
        "kv.lease" => {
            let ttl = i64_or(body, "ttl_ms", 30_000);
            Ok(json!({"lease_id": kv.lease(ttl, &env)}))
        }
        "kv.keep_alive" => {
            let id = text_or(body, "lease_id", "");
            Ok(json!({"expires_at": kv.keep_alive(&id)?}))
        }
        "kv.range" => {
            let prefix = text_or(body, "prefix", "");
            let rows: Vec<Value> = kv
                .range(&env, &prefix)
                .into_iter()
                .map(|(key, value, rev)| json!({"key": key, "value": as_text(&value), "rev": rev}))
                .collect();
            Ok(json!({"count": rows.len(), "rows": rows}))
        }
        "kv.watch" => kv_watch(conn, facades, &env, body),
        other => Err(format!("unknown_method:{other}")),
    }
}

fn kv_watch(conn: &Conn, facades: &Facades, env: &str, body: &Value) -> Answer {
    let prefix = text_or(body, "prefix", "");
    let since_rev = i64_or(body, "since_rev", 0);
    let filter = facades.kv.watch_filter(env, &prefix);
    let subscription = facades.hub.subscribe(filter, SubOpts::default());
    for envelope in facades.kv.watch_snapshot(env, &prefix, since_rev) {
        conn.peer.send(snapshot_frame(&envelope));
    }
    pump(conn.clone(), subscription);
    Ok(json!({"ok": true, "revision": facades.kv.revision()}))
}

fn snapshot_frame(envelope: &Envelope) -> Value {
    let mut frame = envelope.to_value();
    if let Some(object) = frame.as_object_mut() {
        object.insert("t".to_string(), json!("ev"));
        object.insert("snapshot".to_string(), json!(true));
    }
    frame
}

pub fn blob(conn: &Conn, facades: &Facades, method: &str, body: &Value) -> Answer {
    conn.scoped_env(req_env(body).as_deref())?;
    let blob = &facades.blob;
    match method {
        "blob.put" => blob.put(&value_of(body)),
        "blob.get" => {
            let hash = text_or(body, "hash", "");
            let bytes = blob.get(&hash)?;
            Ok(json!({"hash": hash, "size": bytes.len(), "data": b64(&bytes)}))
        }
        "blob.open" => {
            let hash = text_or(body, "hash", "");
            let offset = i64_or(body, "offset", 0);
            let len = i64_or(body, "len", 0);
            let bytes = blob.open(&hash, offset, len)?;
            Ok(json!({"hash": hash, "offset": offset, "len": bytes.len(), "data": b64(&bytes)}))
        }
        "blob.stat" => blob.stat(&text_or(body, "hash", "")),
        other => Err(format!("unknown_method:{other}")),
    }
}

pub fn timer(conn: &Conn, facades: &Facades, method: &str, body: &Value) -> Answer {
    let env = conn.scoped_env(req_env(body).as_deref())?;
    let timer = &facades.timer;
    match method {
        "timer.set" => timer.set(&env, body),
        "timer.list" => Ok(timer.list(&env)),
        // The frame's own `id` is the correlation key on every hop, so a timer's
        // id travels as `timer_id`, the way `approval_id` and `plugin_id` do.
        "timer.del" => Ok(timer.del(&env, &text_or(body, "timer_id", ""))),
        other => Err(format!("unknown_method:{other}")),
    }
}

/// A publish helper the session bridge uses: a durable, model-visible envelope
/// on `session/<kind>`. One code path so P4.1 can delete the duplication.
pub fn bridge_session(hub: &Arc<Hub>, env: &str, kind: &str, data: &Value) {
    let mut envelope = Envelope::new(format!("session/{kind}"), Level::Info, data.clone());
    envelope.env = Some(env.to_string());
    envelope.src = "harness".to_string();
    envelope.model_visible = true;
    hub.emit(envelope);
}

fn value_of(body: &Value) -> Vec<u8> {
    match body.get("data").and_then(Value::as_str) {
        Some(data) => b64_decode(data).unwrap_or_else(|| data.as_bytes().to_vec()),
        None => body
            .get("value")
            .and_then(Value::as_str)
            .map(as_bytes)
            .unwrap_or_default(),
    }
}

fn as_bytes(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

fn as_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn b64_decode(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}
