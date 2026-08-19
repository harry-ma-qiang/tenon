use crate::envelope::Envelope;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// One published envelope, encoded once and shared to every subscriber as an
/// `Arc`. The bytes are the wire form the socket writes; the envelope stays
/// available for server-side coalesce/latest_only/filter decisions.
#[derive(Debug)]
pub struct Published {
    pub envelope: Envelope,
    pub bytes: Arc<[u8]>,
    pub offset: u64,
}

impl Published {
    pub fn new(envelope: Envelope, offset: u64) -> Arc<Self> {
        let bytes: Arc<[u8]> = Arc::from(envelope.encode().into_boxed_slice());
        Arc::new(Self {
            envelope,
            bytes,
            offset,
        })
    }
}

struct State {
    queue: VecDeque<Arc<Published>>,
    dropped: u64,
}

/// The per-subscriber bounded ring of RFC section 4: drop-oldest for
/// non-durable envelopes when full, never-drop for durable ones (they are
/// replayable from the log), and last-per-(topic,key) compaction when the
/// subscriber asked for `latest_only`.
pub struct Ring {
    state: Mutex<State>,
    notify: Notify,
    alive: AtomicBool,
    dropped_total: AtomicU64,
    cap: usize,
    latest_only: bool,
}

impl Ring {
    pub fn new(cap: usize, latest_only: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                dropped: 0,
            }),
            notify: Notify::new(),
            alive: AtomicBool::new(true),
            dropped_total: AtomicU64::new(0),
            cap: cap.max(1),
            latest_only,
        })
    }

    pub fn push(&self, msg: Arc<Published>) {
        {
            let mut state = self.state.lock().expect("ring lock");
            if self.latest_only {
                let key = msg.envelope.compaction_key().to_string();
                state
                    .queue
                    .retain(|existing| existing.envelope.compaction_key() != key);
                state.queue.push_back(msg);
            } else if state.queue.len() >= self.cap && !msg.envelope.durable {
                state.queue.pop_front();
                state.dropped += 1;
                self.dropped_total.fetch_add(1, Ordering::Relaxed);
                state.queue.push_back(msg);
            } else {
                state.queue.push_back(msg);
            }
        }
        self.notify.notify_one();
    }

    pub fn drain(&self) -> Vec<Arc<Published>> {
        let mut state = self.state.lock().expect("ring lock");
        state.dropped = 0;
        state.queue.drain(..).collect()
    }

    fn is_empty(&self) -> bool {
        self.state.lock().expect("ring lock").queue.is_empty()
    }

    pub fn close(&self) {
        self.alive.store(false, Ordering::Relaxed);
        self.notify.notify_one();
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped_total.load(Ordering::Relaxed)
    }
}

/// The stream a `subscribe` hands back. `recv` yields a batch: one envelope
/// each time when the subscriber did not ask to coalesce, and everything that
/// arrived inside the window when it did (the UI's 16 ms frame batching).
pub struct Subscription {
    ring: Arc<Ring>,
    coalesce: Option<Duration>,
    _guard: DropClose,
}

struct DropClose(Arc<Ring>);

impl Drop for DropClose {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Subscription {
    pub(crate) fn new(ring: Arc<Ring>, coalesce_ms: Option<u64>) -> Self {
        Self {
            ring: ring.clone(),
            coalesce: coalesce_ms.filter(|ms| *ms > 0).map(Duration::from_millis),
            _guard: DropClose(ring),
        }
    }

    pub fn ring(&self) -> Arc<Ring> {
        self.ring.clone()
    }

    /// The next batch, or `None` once the ring is closed and drained. Waits for
    /// at least one envelope; with coalescing on, then sleeps the window and
    /// takes everything that piled up so a burst becomes one frame.
    pub async fn recv(&self) -> Option<Vec<Arc<Published>>> {
        loop {
            if !self.ring.is_empty() {
                if let Some(window) = self.coalesce {
                    tokio::time::sleep(window).await;
                }
                let batch = self.ring.drain();
                if !batch.is_empty() {
                    return Some(batch);
                }
            }
            if !self.ring.alive() && self.ring.is_empty() {
                return None;
            }
            self.ring.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Envelope, Level};
    use serde_json::json;

    fn msg(topic: &str, durable: bool, key: Option<&str>) -> Arc<Published> {
        let mut env = Envelope::new(topic, Level::Info, json!({}));
        env.durable = durable;
        if let Some(key) = key {
            env.tags.insert("key".to_string(), key.to_string());
        }
        Published::new(env, 0)
    }

    #[test]
    fn drop_oldest_when_full_for_non_durable() {
        let ring = Ring::new(2, false);
        ring.push(msg("a", false, None));
        ring.push(msg("b", false, None));
        ring.push(msg("c", false, None));
        let batch = ring.drain();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].envelope.topic, "b");
        assert_eq!(batch[1].envelope.topic, "c");
        assert_eq!(ring.dropped(), 1);
    }

    #[test]
    fn durable_never_dropped_even_over_cap() {
        let ring = Ring::new(2, false);
        ring.push(msg("a", true, None));
        ring.push(msg("b", true, None));
        ring.push(msg("c", true, None));
        assert_eq!(ring.drain().len(), 3);
        assert_eq!(ring.dropped(), 0);
    }

    #[test]
    fn latest_only_keeps_last_per_key() {
        let ring = Ring::new(100, true);
        ring.push(msg("metrics/cpu", false, Some("host1")));
        ring.push(msg("metrics/cpu", false, Some("host2")));
        ring.push(msg("metrics/cpu", false, Some("host1")));
        let batch = ring.drain();
        assert_eq!(batch.len(), 2, "one row per key");
    }
}
