use crate::envelope::Envelope;
use crate::filter::Filter;
use crate::ring::{Published, Ring, Subscription};
use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// The durable side of the fabric, provided by the host: an append that returns
/// the log offset of each row and a replay of everything after an offset. The
/// hub never sees SQLite; the host hides its single non-`Sync` connection
/// behind a mutex inside this trait.
pub trait Durable: Send + Sync {
    fn append_batch(&self, batch: &[Envelope]) -> Result<Vec<u64>, String>;
    fn since(&self, after: i64, limit: i64) -> Result<Vec<(u64, Envelope)>, String>;
    /// The highest offset already in the log. Read once at boot so offsets and
    /// the replay ceiling continue across a restart rather than resetting to 0.
    fn head(&self) -> u64;
}

/// The group-commit tick of RFC section 10.1: a durable publish resolves after
/// the batch that carries it is persisted.
const GROUP_COMMIT: Duration = Duration::from_millis(5);
const BATCH_MAX: usize = 512;
const DEFAULT_CAP: usize = 4096;
const REPLAY_MAX: i64 = 50_000;

struct SubEntry {
    filter: Filter,
    ring: Arc<Ring>,
    since_max: u64,
}

struct Job {
    envelope: Envelope,
    reply: Option<oneshot::Sender<u64>>,
}

/// Options a `subscribe` carries beyond its filter (RFC section 3/4).
#[derive(Debug, Clone, Default)]
pub struct SubOpts {
    pub since_offset: Option<i64>,
    pub coalesce_ms: Option<u64>,
    pub latest_only: bool,
    pub capacity: Option<usize>,
}

/// The in-base message hub: lock-free fan-out over an `ArcSwap` snapshot of the
/// subscriber list, per-subscriber rings, one durable writer task with a 5 ms
/// group commit. Non-durable envelopes never touch the writer.
pub struct Hub {
    subs: ArcSwap<Vec<Arc<SubEntry>>>,
    write_lock: Mutex<()>,
    durable: Option<Arc<dyn Durable>>,
    writer: Mutex<Option<mpsc::UnboundedSender<Job>>>,
    max_offset: AtomicU64,
    #[cfg(feature = "http")]
    guard: crate::secret::SecretGuard,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::new(Hub {
            subs: ArcSwap::from_pointee(Vec::new()),
            write_lock: Mutex::new(()),
            durable: None,
            writer: Mutex::new(None),
            max_offset: AtomicU64::new(0),
            #[cfg(feature = "http")]
            guard: crate::secret::SecretGuard::new(),
        })
    }

    /// A hub whose durable topics are written to and replayed from `durable`.
    /// Spawns the single writer task; the task holds only a `Weak` to the hub,
    /// so a dropped hub stops it.
    pub fn with_durable(durable: Arc<dyn Durable>) -> Arc<Hub> {
        let head = durable.head();
        let hub = Arc::new(Hub {
            subs: ArcSwap::from_pointee(Vec::new()),
            write_lock: Mutex::new(()),
            durable: Some(durable),
            writer: Mutex::new(None),
            max_offset: AtomicU64::new(head),
            #[cfg(feature = "http")]
            guard: crate::secret::SecretGuard::new(),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        *hub.writer.lock().expect("writer lock") = Some(tx);
        let weak = Arc::downgrade(&hub);
        tokio::spawn(writer_loop(rx, weak));
        hub
    }

    pub fn max_offset(&self) -> u64 {
        self.max_offset.load(Ordering::Relaxed)
    }

    /// Install the current secret set (RFC 8d.4). Base is the only caller; the
    /// hub then scrubs every payload it fans out or persists.
    #[cfg(feature = "http")]
    pub fn set_secrets(&self, rules: Vec<crate::secret::Rule>) {
        self.guard.set(rules);
    }

    /// The single scrub any producer that writes its own durable copy (base's
    /// event-log append) calls before that write, so the state file never holds
    /// a raw secret value. `Err(name)` means a `block` secret matched.
    #[cfg(feature = "http")]
    pub fn scrub(&self, payload: &mut serde_json::Value) -> Result<(), String> {
        self.guard.scan(payload)
    }

    /// Runs the leak guard over an envelope's payload. On a `block` match it fans
    /// out a `guardian/violation` event (value-free) and reports the error; on
    /// `mask` the payload is rewritten in place and the envelope proceeds.
    #[cfg(feature = "http")]
    fn guard(&self, envelope: &mut Envelope) -> Result<(), String> {
        if self.guard.is_empty() {
            return Ok(());
        }
        match self
            .guard
            .scan_envelope(&mut envelope.payload, &mut envelope.tags)
        {
            Ok(()) => Ok(()),
            Err(name) => {
                self.emit_violation(&name, &envelope.topic, envelope.env.clone());
                Err(format!(
                    "secret {name} blocked from topic {}",
                    envelope.topic
                ))
            }
        }
    }

    #[cfg(feature = "http")]
    fn emit_violation(&self, name: &str, topic: &str, env: Option<String>) {
        let mut violation = Envelope::new(
            "guardian/violation",
            crate::envelope::Level::Error,
            serde_json::json!({"secret": name, "topic": topic, "action": "block"}),
        );
        violation.env = env;
        violation.src = "base".to_string();
        violation.normalize();
        self.fan_out(Published::new(violation, 0));
    }

    fn writer_tx(&self) -> Option<mpsc::UnboundedSender<Job>> {
        self.writer.lock().expect("writer lock").clone()
    }

    /// Fire-and-forget publish for producers that cannot await (the tracing
    /// layer). Durable envelopes still go through the writer; the caller just
    /// does not learn the offset.
    pub fn emit(&self, mut envelope: Envelope) {
        envelope.normalize();
        #[cfg(feature = "http")]
        if self.guard(&mut envelope).is_err() {
            return;
        }
        if envelope.durable {
            if let Some(tx) = self.writer_tx() {
                let _ = tx.send(Job {
                    envelope,
                    reply: None,
                });
                return;
            }
        }
        self.fan_out(Published::new(envelope, 0));
    }

    /// Publish and wait: a durable envelope resolves to its log offset after the
    /// group-commit batch is persisted; a non-durable one fans out in memory and
    /// resolves to 0.
    pub async fn publish(&self, mut envelope: Envelope) -> Result<u64, String> {
        envelope.normalize();
        #[cfg(feature = "http")]
        self.guard(&mut envelope)?;
        if envelope.durable {
            if let Some(tx) = self.writer_tx() {
                let (reply, wait) = oneshot::channel();
                tx.send(Job {
                    envelope,
                    reply: Some(reply),
                })
                .map_err(|_| "bus writer gone".to_string())?;
                return wait.await.map_err(|_| "bus writer gone".to_string());
            }
        }
        self.fan_out(Published::new(envelope, 0));
        Ok(0)
    }

    fn fan_out(&self, msg: Arc<Published>) {
        if msg.envelope.is_expired(crate::envelope::now_ms()) {
            return;
        }
        let subs = self.subs.load();
        let mut dead = false;
        for sub in subs.iter() {
            if !sub.ring.alive() {
                dead = true;
                continue;
            }
            if msg.offset > 0 && msg.offset <= sub.since_max {
                continue;
            }
            if sub.filter.matches(&msg.envelope) {
                sub.ring.push(msg.clone());
            }
        }
        if dead {
            self.prune();
        }
    }

    fn prune(&self) {
        let _guard = self.write_lock.lock().expect("sub lock");
        let kept: Vec<Arc<SubEntry>> = self
            .subs
            .load()
            .iter()
            .filter(|sub| sub.ring.alive())
            .cloned()
            .collect();
        self.subs.store(Arc::new(kept));
    }

    /// Register a subscriber and, when it asked for `since_offset`, replay the
    /// durable log after that offset into its ring before any live envelope.
    pub fn subscribe(&self, filter: Filter, opts: SubOpts) -> Subscription {
        let ring = Ring::new(opts.capacity.unwrap_or(DEFAULT_CAP), opts.latest_only);
        let ceiling = self.max_offset.load(Ordering::Relaxed);
        let entry = Arc::new(SubEntry {
            filter: filter.clone(),
            ring: ring.clone(),
            since_max: ceiling,
        });
        {
            let _guard = self.write_lock.lock().expect("sub lock");
            let mut next: Vec<Arc<SubEntry>> = self
                .subs
                .load()
                .iter()
                .filter(|sub| sub.ring.alive())
                .cloned()
                .collect();
            next.push(entry);
            self.subs.store(Arc::new(next));
        }
        if let (Some(after), Some(durable)) = (opts.since_offset, self.durable.as_ref()) {
            if let Ok(rows) = durable.since(after, REPLAY_MAX) {
                let now = crate::envelope::now_ms();
                for (offset, envelope) in rows {
                    if offset > ceiling {
                        continue;
                    }
                    if envelope.is_expired(now) {
                        continue;
                    }
                    if filter.matches(&envelope) {
                        ring.push(Published::new(envelope, offset));
                    }
                }
            }
        }
        Subscription::new(ring, opts.coalesce_ms)
    }

    pub fn subscriber_count(&self) -> usize {
        self.subs.load().iter().filter(|s| s.ring.alive()).count()
    }
}

async fn writer_loop(mut rx: mpsc::UnboundedReceiver<Job>, hub: Weak<Hub>) {
    while let Some(first) = rx.recv().await {
        let mut jobs = vec![first];
        let deadline = tokio::time::sleep(GROUP_COMMIT);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                job = rx.recv() => match job {
                    Some(job) => {
                        jobs.push(job);
                        if jobs.len() >= BATCH_MAX {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
        let Some(hub) = hub.upgrade() else {
            return;
        };
        let envelopes: Vec<Envelope> = jobs.iter().map(|job| job.envelope.clone()).collect();
        let offsets = match hub.durable.as_ref() {
            Some(durable) => durable.append_batch(&envelopes).unwrap_or_default(),
            None => Vec::new(),
        };
        if let Some(top) = offsets.iter().copied().max() {
            hub.max_offset.fetch_max(top, Ordering::Relaxed);
        }
        for (index, job) in jobs.into_iter().enumerate() {
            let offset = offsets.get(index).copied().unwrap_or(0);
            hub.fan_out(Published::new(job.envelope, offset));
            if let Some(reply) = job.reply {
                let _ = reply.send(offset);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Envelope, Level};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct MemLog {
        rows: StdMutex<Vec<Envelope>>,
    }

    impl Durable for MemLog {
        fn append_batch(&self, batch: &[Envelope]) -> Result<Vec<u64>, String> {
            let mut rows = self.rows.lock().unwrap();
            let mut offsets = Vec::new();
            for env in batch {
                rows.push(env.clone());
                offsets.push(rows.len() as u64);
            }
            Ok(offsets)
        }

        fn since(&self, after: i64, limit: i64) -> Result<Vec<(u64, Envelope)>, String> {
            let rows = self.rows.lock().unwrap();
            Ok(rows
                .iter()
                .enumerate()
                .map(|(index, env)| (index as u64 + 1, env.clone()))
                .filter(|(offset, _)| *offset as i64 > after)
                .take(limit.max(0) as usize)
                .collect())
        }

        fn head(&self) -> u64 {
            self.rows.lock().unwrap().len() as u64
        }
    }

    fn env(topic: &str, durable: bool) -> Envelope {
        let mut env = Envelope::new(topic, Level::Info, json!({"t": topic}));
        env.durable = durable;
        env
    }

    #[tokio::test]
    async fn non_durable_roundtrip_with_filter() {
        let hub = Hub::new();
        let sub = hub.subscribe(
            Filter {
                topics: vec!["session/*".to_string()],
                ..Filter::default()
            },
            SubOpts::default(),
        );
        hub.emit(env("session/tool", false));
        hub.emit(env("internal/tick", false));
        let batch = sub.recv().await.expect("a batch");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].envelope.topic, "session/tool");
    }

    #[tokio::test]
    async fn durable_publish_persists_and_assigns_offset() {
        let log = Arc::new(MemLog::default());
        let hub = Hub::with_durable(log.clone());
        let sub = hub.subscribe(Filter::all(), SubOpts::default());
        let offset = hub.publish(env("session/a", true)).await.expect("publish");
        assert_eq!(offset, 1);
        let batch = sub.recv().await.expect("batch");
        assert_eq!(batch[0].offset, 1);
        assert_eq!(log.rows.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn since_offset_replays_missed_durable_envelopes() {
        let log = Arc::new(MemLog::default());
        let hub = Hub::with_durable(log.clone());
        hub.publish(env("session/a", true)).await.unwrap();
        hub.publish(env("session/b", true)).await.unwrap();
        let sub = hub.subscribe(
            Filter::all(),
            SubOpts {
                since_offset: Some(0),
                ..SubOpts::default()
            },
        );
        let batch = sub.recv().await.expect("replay batch");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].envelope.topic, "session/a");
        assert_eq!(batch[1].envelope.topic, "session/b");
    }
}
