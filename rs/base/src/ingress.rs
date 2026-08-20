use crate::base::Base;
use crate::bus::Facades;
use crate::facaderpc::Conn;
use crate::kv::KvFacade;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

type Answer = Result<Value, String>;

/// The reserved kv namespace every ingress route lives under (RFC 8c, P4.5).
/// One namespace, host-global, so a name is unique across the whole env tree; a
/// scoped app can never read or write it because the facade authorizer pins a
/// scoped caller to its own env, never `@ingress`.
pub const NS: &str = "@ingress";

/// The container-side base port an env's apps bind. The sandbox publishes the
/// span `[BASE, BASE + ingress.max_per_env)` for each env; an app picks one and
/// names it in `ingress.register`.
pub const INGRESS_CPORT_BASE: u16 = 18080;

const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
const PROBE_MISSES: u32 = 2;

fn key_of(name: &str) -> String {
    format!("/ingress/{name}")
}

fn name_of(key: &str) -> String {
    key.trim_start_matches("/ingress/").to_string()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The stored route as JSON: `env` owns it, `addr` is the host-reachable
/// `host:port`, `public` waives the bearer token, `port` is the container port
/// and `lease` is the kv lease keeping the key alive.
fn route_json(env: &str, addr: &str, public: bool, port: i64, lease: &str) -> Value {
    json!({"env": env, "addr": addr, "public": public, "port": port, "lease": lease})
}

pub fn route_of(kv: &KvFacade, name: &str) -> Option<Value> {
    let (bytes, _rev) = kv.get(NS, &key_of(name))?;
    serde_json::from_slice(&bytes).ok()
}

fn routes(kv: &KvFacade) -> Vec<(String, Value)> {
    kv.range(NS, "/ingress/")
        .into_iter()
        .filter_map(|(key, bytes, _rev)| {
            serde_json::from_slice::<Value>(&bytes)
                .ok()
                .map(|value| (name_of(&key), value))
        })
        .collect()
}

/// `ingress.list` and `tenon ingress`: the routes a caller may see. A scoped app
/// sees only its own env's routes (RFC 8d.2); base and the CLI see every env, or
/// one named env.
pub fn list(facades: &Facades, scope: Option<String>, requested: Option<String>) -> Answer {
    let filter = scope.or(requested);
    let rows: Vec<Value> = routes(&facades.kv)
        .into_iter()
        .filter(|(_, value)| match &filter {
            Some(env) => value.get("env").and_then(Value::as_str) == Some(env.as_str()),
            None => true,
        })
        .map(|(name, value)| {
            json!({
                "name": name,
                "env": value.get("env").cloned().unwrap_or(Value::Null),
                "addr": value.get("addr").cloned().unwrap_or(Value::Null),
                "public": value.get("public").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    Ok(json!({"count": rows.len(), "routes": rows}))
}

/// `ingress.resolve`: what `serve` asks base before it proxies `/app/<name>`.
/// The route's host address and whether it is public; a scoped caller is denied
/// a name it does not own.
pub fn resolve(facades: &Facades, conn: &Conn, name: &str) -> Answer {
    match route_of(&facades.kv, name) {
        Some(value) => {
            if let Some(env) = conn.bound_scope() {
                if value.get("env").and_then(Value::as_str) != Some(env.as_str()) {
                    return Err("cross_env_denied".to_string());
                }
            }
            Ok(json!({
                "found": true,
                "addr": value.get("addr").cloned().unwrap_or(Value::Null),
                "public": value.get("public").and_then(Value::as_bool).unwrap_or(false),
                "env": value.get("env").cloned().unwrap_or(Value::Null),
            }))
        }
        None => Ok(json!({"found": false})),
    }
}

impl Base {
    pub fn ingress_register(
        &mut self,
        peer: u64,
        name: String,
        port: i64,
        public: bool,
        approved: bool,
        reply: oneshot::Sender<Answer>,
    ) {
        let outcome = self.ingress_check(peer, &name, port);
        let (env, addr) = match outcome {
            Ok(pair) => pair,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let gated = self
            .config
            .approval
            .gated_tools
            .iter()
            .any(|tool| tool == "ingress.register");
        if !approved && gated {
            let reason = format!("ingress.register {name} in env {env}");
            self.gate(
                &env.clone(),
                "ingress.register",
                &reason,
                reply,
                move |reply| Cmd::IngressRegister {
                    peer,
                    name,
                    port,
                    public,
                    approved: true,
                    reply,
                },
            );
            return;
        }
        let _ = reply.send(self.ingress_commit(&env, &name, &addr, port, public));
    }

    /// Validation that must precede any write: the caller owns an env, the name
    /// is well-formed and free of any other env, the env is inside quota, and the
    /// port was published for this env's sandbox. Returns the env and the
    /// host-reachable address to write.
    fn ingress_check(&self, peer: u64, name: &str, port: i64) -> Result<(String, String), String> {
        if !valid_name(name) {
            return Err("ingress name must be 1-64 chars of [A-Za-z0-9_-]".to_string());
        }
        let Some(env) = self.env_of_peer(peer) else {
            return Err("ingress.register is only for an app inside a sandbox".to_string());
        };
        let Some(kv) = self.kv.as_ref() else {
            return Err("facades_unavailable".to_string());
        };
        if let Some(existing) = route_of(kv, name) {
            let owner = existing.get("env").and_then(Value::as_str).unwrap_or("");
            if owner != env {
                return Err(format!("ingress name {name} is owned by env {owner}"));
            }
        } else {
            let all = routes(kv);
            if all.len() >= self.config.ingress.max_total {
                return Err("ingress host quota reached".to_string());
            }
            let mine = all
                .iter()
                .filter(|(_, value)| value.get("env").and_then(Value::as_str) == Some(env.as_str()))
                .count();
            if mine >= self.config.ingress.max_per_env {
                return Err(format!("ingress quota for env {env} reached"));
            }
        }
        let port = u16::try_from(port).map_err(|_| "ingress port out of range".to_string())?;
        let addr = self
            .nodes
            .get(&env)
            .and_then(|node| node.sandbox.as_ref())
            .and_then(|instance| instance.ingress_addr(port))
            .ok_or_else(|| format!("ingress port {port} is not published for env {env}"))?;
        Ok((env, addr))
    }

    fn ingress_commit(
        &mut self,
        env: &str,
        name: &str,
        addr: &str,
        port: i64,
        public: bool,
    ) -> Answer {
        let ttl = self.config.ingress.lease_ttl_ms;
        let Some(kv) = self.kv.clone() else {
            return Err("facades_unavailable".to_string());
        };
        let lease = kv.lease(ttl, NS);
        let value = route_json(env, addr, public, port, &lease);
        kv.set(
            NS,
            &key_of(name),
            value.to_string().into_bytes(),
            false,
            None,
            Some(lease.clone()),
        )?;
        self.emit_env(
            env,
            "ingress.register",
            json!({"name": name, "addr": addr, "public": public, "port": port}),
        );
        Ok(json!({
            "ok": true,
            "name": name,
            "env": env,
            "addr": addr,
            "public": public,
            "lease_id": lease,
            "ttl_ms": ttl,
        }))
    }

    pub fn ingress_unregister(&mut self, peer: u64, name: String, reply: oneshot::Sender<Answer>) {
        let _ = reply.send(self.ingress_drop(peer, &name));
    }

    fn ingress_drop(&mut self, peer: u64, name: &str) -> Answer {
        let Some(env) = self.env_of_peer(peer) else {
            return Err("ingress.unregister is only for an app inside a sandbox".to_string());
        };
        let Some(kv) = self.kv.clone() else {
            return Err("facades_unavailable".to_string());
        };
        match route_of(&kv, name) {
            Some(value) if value.get("env").and_then(Value::as_str) == Some(env.as_str()) => {
                kv.del(NS, &key_of(name));
                self.emit_env(env.as_str(), "ingress.unregister", json!({"name": name}));
                Ok(json!({"ok": true, "name": name}))
            }
            Some(_) => Err(format!("ingress name {name} is not owned by env {env}")),
            None => Ok(json!({"ok": false, "name": name, "reason": "no such route"})),
        }
    }
}

/// The liveness loop (RFC 8c: "app dies -> route expires"). Base keeps a live
/// route's lease renewed by probing the app's own address, so a plain server
/// that only registers and serves gets automatic expiry without embedding a
/// keep-alive client. Two consecutive failed probes drop the route at once; the
/// lease TTL is the backstop if base itself cannot probe.
pub fn spawn_liveness(kv: Arc<KvFacade>, probe_ms: u64) {
    tokio::spawn(async move {
        let interval = Duration::from_millis(probe_ms.max(200));
        let mut misses: HashMap<String, u32> = HashMap::new();
        loop {
            tokio::time::sleep(interval).await;
            let current = routes(&kv);
            let live: std::collections::HashSet<String> =
                current.iter().map(|(name, _)| name.clone()).collect();
            misses.retain(|name, _| live.contains(name));
            for (name, value) in current {
                let addr = value.get("addr").and_then(Value::as_str).unwrap_or("");
                let lease = value.get("lease").and_then(Value::as_str).unwrap_or("");
                if probe(addr).await {
                    misses.remove(&name);
                    let _ = kv.keep_alive(lease);
                } else {
                    let count = misses.entry(name.clone()).or_insert(0);
                    *count += 1;
                    if *count >= PROBE_MISSES {
                        kv.del(NS, &key_of(&name));
                        misses.remove(&name);
                    }
                }
            }
        }
    });
}

/// One liveness probe: connect and ask for `/`, count any HTTP bytes back as
/// alive. A connect that a port forwarder accepts but the dead app resets (the
/// oci case) reads zero bytes and counts as dead, which a bare `connect` would
/// miss.
async fn probe(addr: &str) -> bool {
    if addr.is_empty() {
        return false;
    }
    let attempt = async {
        let mut stream = TcpStream::connect(addr).await.ok()?;
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: tenon-ingress\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut buffer = [0u8; 16];
        let read = stream.read(&mut buffer).await.ok()?;
        (read > 0).then_some(())
    };
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, attempt).await,
        Ok(Some(()))
    )
}
