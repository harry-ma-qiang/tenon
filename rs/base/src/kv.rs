use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tenon_bus::{Envelope, Filter, Hub, Level};
use tenon_storage::{now, Store};

const EXPIRY_TICK_MS: u64 = 1000;

#[derive(Clone)]
struct Cell {
    value: Vec<u8>,
    rev: i64,
    expires_at: Option<i64>,
    lease_id: Option<String>,
}

struct Lease {
    ttl_ms: i64,
    expires_at: i64,
    env: String,
}

/// RFC section 3's kv facade and RFC section 7's cluster-ready seam: an
/// etcd-lite over a global monotonic `revision`, ephemeral keys in memory,
/// durable keys in the `kv` table, leases, and a `watch` that rides the bus as
/// `kv/<key>` envelopes. Every key is env-scoped (RFC 8d.2): a caller only ever
/// names keys inside its own env.
pub struct KvFacade {
    store: Arc<Mutex<Store>>,
    hub: Arc<Hub>,
    revision: AtomicI64,
    reserved: AtomicI64,
    ephemeral: RwLock<HashMap<(String, String), Cell>>,
    leases: RwLock<HashMap<String, Lease>>,
    lease_seq: AtomicI64,
}

/// How far ahead the revision high-water is persisted each time the counter
/// crosses the reserved ceiling: one durable write per block, not per bump, so
/// ephemeral bumps stay cheap while a `kill -9` loses at most a block of unused
/// revisions (a gap, never a rewind — the counter stays monotonic).
const REV_BLOCK: i64 = 256;

/// A key change as `watch`/the change envelope carries it.
pub struct Change {
    pub op: &'static str,
    pub key: String,
    pub value: Option<Vec<u8>>,
    pub rev: i64,
}

impl KvFacade {
    pub fn new(store: Arc<Mutex<Store>>, hub: Arc<Hub>) -> Arc<KvFacade> {
        let start = {
            let store = store.lock().expect("kv store");
            store
                .kv_max_rev()
                .unwrap_or(0)
                .max(store.kv_rev_hwm().unwrap_or(0))
        };
        let facade = Arc::new(KvFacade {
            store,
            hub,
            revision: AtomicI64::new(start),
            reserved: AtomicI64::new(start),
            ephemeral: RwLock::new(HashMap::new()),
            leases: RwLock::new(HashMap::new()),
            lease_seq: AtomicI64::new(0),
        });
        spawn_expiry(Arc::downgrade(&facade));
        facade
    }

    pub fn revision(&self) -> i64 {
        self.revision.load(Ordering::Relaxed)
    }

    /// Every revision bump — durable, ephemeral or delete — advances one global
    /// counter and, when it crosses the reserved ceiling, persists a new
    /// high-water block. Restart then seeds from that ceiling, so no revision an
    /// ephemeral write consumed is ever reissued (RFC section 3's monotonicity).
    fn next_rev(&self) -> i64 {
        let rev = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        if rev > self.reserved.load(Ordering::Relaxed) {
            let ceiling = rev + REV_BLOCK;
            let _ = self.store.lock().expect("kv store").kv_set_rev_hwm(ceiling);
            self.reserved.fetch_max(ceiling, Ordering::Relaxed);
        }
        rev
    }

    pub fn get(&self, env: &str, key: &str) -> Option<(Vec<u8>, i64)> {
        if let Some(cell) = self.ephemeral.read().expect("kv").get(&owned(env, key)) {
            if expired(cell.expires_at) {
                return None;
            }
            return Some((cell.value.clone(), cell.rev));
        }
        let row = self
            .store
            .lock()
            .expect("kv store")
            .kv_get(env, key)
            .ok()
            .flatten()?;
        if expired(row.expires_at) {
            return None;
        }
        Some((row.value, row.rev))
    }

    /// `set` with the durability, ttl and lease of RFC section 3. A durable set
    /// clears any ephemeral shadow of the key and vice versa, so a key lives in
    /// exactly one place. Fires a `kv/<key>` watch envelope.
    pub fn set(
        &self,
        env: &str,
        key: &str,
        value: Vec<u8>,
        durable: bool,
        ttl_ms: Option<i64>,
        lease_id: Option<String>,
    ) -> Result<i64, String> {
        let rev = self.next_rev();
        let expires_at = ttl_ms.map(|ms| now() + ms.max(0));
        self.write(
            env,
            key,
            durable,
            Cell {
                value: value.clone(),
                rev,
                expires_at,
                lease_id,
            },
        )?;
        self.fire(
            env,
            Change {
                op: "set",
                key: key.to_string(),
                value: Some(value),
                rev,
            },
        );
        Ok(rev)
    }

    fn write(&self, env: &str, key: &str, durable: bool, cell: Cell) -> Result<i64, String> {
        let rev = cell.rev;
        if durable {
            self.ephemeral.write().expect("kv").remove(&owned(env, key));
            self.store
                .lock()
                .expect("kv store")
                .kv_set(
                    env,
                    key,
                    &cell.value,
                    rev,
                    cell.expires_at,
                    cell.lease_id.as_deref(),
                )
                .map_err(|error| error.to_string())?;
        } else {
            let _ = self.store.lock().expect("kv store").kv_del(env, key);
            self.ephemeral
                .write()
                .expect("kv")
                .insert(owned(env, key), cell);
        }
        Ok(rev)
    }

    pub fn del(&self, env: &str, key: &str) -> bool {
        let ephemeral = self.ephemeral.write().expect("kv").remove(&owned(env, key));
        let durable = self
            .store
            .lock()
            .expect("kv store")
            .kv_del(env, key)
            .unwrap_or(false);
        let gone = ephemeral.is_some() || durable;
        if gone {
            let rev = self.next_rev();
            self.fire(
                env,
                Change {
                    op: "del",
                    key: key.to_string(),
                    value: None,
                    rev,
                },
            );
        }
        gone
    }

    /// Compare-and-swap: succeeds only when the current value equals `expect`
    /// (or both are absent). The read and the write are one critical section per
    /// backing store, so two racing writers cannot both win.
    pub fn cas(
        &self,
        env: &str,
        key: &str,
        expect: Option<Vec<u8>>,
        value: Vec<u8>,
        durable: bool,
    ) -> Result<i64, String> {
        let _guard = self.cas_lock(env);
        let current = self.get(env, key).map(|(bytes, _)| bytes);
        if current != expect {
            return Err("cas_mismatch".to_string());
        }
        self.set(env, key, value, durable, None, None)
    }

    pub fn incr(&self, env: &str, key: &str, delta: i64, durable: bool) -> Result<i64, String> {
        let _guard = self.cas_lock(env);
        let current = match self.get(env, key) {
            Some((bytes, _)) => String::from_utf8_lossy(&bytes)
                .trim()
                .parse::<i64>()
                .map_err(|_| "incr on a non-integer value".to_string())?,
            None => 0,
        };
        let next = current + delta;
        self.set(env, key, next.to_string().into_bytes(), durable, None, None)?;
        Ok(next)
    }

    pub fn expire(&self, env: &str, key: &str, ttl_ms: i64) -> Result<i64, String> {
        let (value, _) = self.get(env, key).ok_or("unknown key")?;
        let durable = self
            .store
            .lock()
            .expect("kv store")
            .kv_get(env, key)
            .ok()
            .flatten()
            .is_some();
        let rev = self.next_rev();
        self.write(
            env,
            key,
            durable,
            Cell {
                value,
                rev,
                expires_at: Some(now() + ttl_ms.max(0)),
                lease_id: None,
            },
        )?;
        Ok(rev)
    }

    pub fn lease(&self, ttl_ms: i64, env: &str) -> String {
        let id = format!(
            "L{}-{}",
            std::process::id(),
            self.lease_seq.fetch_add(1, Ordering::Relaxed)
        );
        self.leases.write().expect("leases").insert(
            id.clone(),
            Lease {
                ttl_ms: ttl_ms.max(0),
                expires_at: now() + ttl_ms.max(0),
                env: env.to_string(),
            },
        );
        id
    }

    pub fn keep_alive(&self, lease_id: &str) -> Result<i64, String> {
        let mut leases = self.leases.write().expect("leases");
        let lease = leases.get_mut(lease_id).ok_or("unknown lease")?;
        lease.expires_at = now() + lease.ttl_ms;
        Ok(lease.expires_at)
    }

    pub fn range(&self, env: &str, prefix: &str) -> Vec<(String, Vec<u8>, i64)> {
        let mut out: HashMap<String, (Vec<u8>, i64)> = HashMap::new();
        if let Ok(rows) = self.store.lock().expect("kv store").kv_range(env, prefix) {
            for row in rows {
                if !expired(row.expires_at) {
                    out.insert(row.key, (row.value, row.rev));
                }
            }
        }
        for ((cell_env, key), cell) in self.ephemeral.read().expect("kv").iter() {
            if cell_env == env && key.starts_with(prefix) && !expired(cell.expires_at) {
                out.insert(key.clone(), (cell.value.clone(), cell.rev));
            }
        }
        let mut rows: Vec<(String, Vec<u8>, i64)> = out
            .into_iter()
            .map(|(key, (value, rev))| (key, value, rev))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// The bus filter a `watch(prefix)` subscribes with: `kv/<prefix>**`,
    /// env-scoped. Segment-aligned — prefixes are path-like (`/timers/`,
    /// `/ctl/`).
    pub fn watch_filter(&self, env: &str, prefix: &str) -> Filter {
        Filter {
            topics: vec![format!("kv/{}**", prefix.trim_start_matches('/'))],
            env: Some(env.to_string()),
            ..Filter::default()
        }
    }

    /// The since_rev snapshot a watch replays before going live: current keys
    /// under `prefix` whose rev is newer than the watcher last saw.
    pub fn watch_snapshot(&self, env: &str, prefix: &str, since_rev: i64) -> Vec<Envelope> {
        self.range(env, prefix)
            .into_iter()
            .filter(|(_, _, rev)| *rev > since_rev)
            .map(|(key, value, rev)| change_envelope(env, "set", &key, Some(&value), rev))
            .collect()
    }

    /// Durable keys under `prefix` across every env, as `(env, key, value)`.
    /// Base-internal (the timer wheel); no scoped caller reaches it.
    pub fn scan_all(&self, prefix: &str) -> Vec<(String, String, Vec<u8>)> {
        self.store
            .lock()
            .expect("kv store")
            .kv_scan(prefix)
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.env, row.key, row.value))
            .collect()
    }

    fn fire(&self, env: &str, change: Change) {
        let envelope = change_envelope(
            env,
            change.op,
            &change.key,
            change.value.as_deref(),
            change.rev,
        );
        self.hub.emit(envelope);
    }

    fn cas_lock(&self, _env: &str) -> std::sync::MutexGuard<'_, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().expect("cas lock")
    }

    /// One expiry sweep: keys past their TTL, then leases past theirs (which
    /// delete every key bound to them). Every removal fires a watch event.
    pub fn tick(&self, at: i64) {
        let mut doomed: Vec<(String, String)> = Vec::new();
        if let Ok(rows) = self.store.lock().expect("kv store").kv_expired(at) {
            for (env, key, _) in rows {
                doomed.push((env, key));
            }
        }
        for ((env, key), cell) in self.ephemeral.read().expect("kv").iter() {
            if cell.expires_at.map(|e| e <= at).unwrap_or(false) {
                doomed.push((env.clone(), key.clone()));
            }
        }
        let dead_leases: Vec<(String, String)> = self
            .leases
            .read()
            .expect("leases")
            .iter()
            .filter(|(_, lease)| lease.expires_at <= at)
            .map(|(id, lease)| (id.clone(), lease.env.clone()))
            .collect();
        for (lease_id, _) in &dead_leases {
            if let Ok(rows) = self.store.lock().expect("kv store").kv_by_lease(lease_id) {
                doomed.extend(rows);
            }
            let bound: Vec<(String, String)> = self
                .ephemeral
                .read()
                .expect("kv")
                .iter()
                .filter(|(_, cell)| cell.lease_id.as_deref() == Some(lease_id.as_str()))
                .map(|((env, key), _)| (env.clone(), key.clone()))
                .collect();
            doomed.extend(bound);
            self.leases.write().expect("leases").remove(lease_id);
        }
        doomed.sort();
        doomed.dedup();
        for (env, key) in doomed {
            self.del(&env, &key);
        }
    }
}

fn spawn_expiry(facade: Weak<KvFacade>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(EXPIRY_TICK_MS)).await;
            match facade.upgrade() {
                Some(facade) => facade.tick(now()),
                None => return,
            }
        }
    });
}

fn change_envelope(env: &str, op: &str, key: &str, value: Option<&[u8]>, rev: i64) -> Envelope {
    let mut envelope = Envelope::new(
        format!("kv/{}", key.trim_start_matches('/')),
        Level::Info,
        json!({
            "op": op,
            "key": key,
            "rev": rev,
            "value": value.map(|bytes| String::from_utf8_lossy(bytes).into_owned()),
        }),
    );
    envelope.env = Some(env.to_string());
    envelope.src = "kv".to_string();
    envelope.durable = true;
    envelope
}

fn owned(env: &str, key: &str) -> (String, String) {
    (env.to_string(), key.to_string())
}

fn expired(expires_at: Option<i64>) -> bool {
    expires_at.map(|at| at <= now()).unwrap_or(false)
}
