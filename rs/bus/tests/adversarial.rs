use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tenon_bus::{glob, Durable, Envelope, Filter, Hub, Level, SubOpts};

#[derive(Default)]
struct MemLog {
    rows: Mutex<Vec<Envelope>>,
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

fn env(topic: &str, durable: bool, payload: serde_json::Value) -> Envelope {
    let mut env = Envelope::new(topic, Level::Info, payload);
    env.durable = durable;
    env
}

/// RFC section 4: a bounded ring drops the OLDEST non-durable envelope when a
/// slow subscriber falls behind a fast publisher, but a durable topic is
/// never dropped from the ring itself (it also survives via log replay).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_subscriber_drops_oldest_non_durable_but_keeps_every_durable() {
    let log = Arc::new(MemLog::default());
    let hub = Hub::with_durable(log.clone());
    let sub = hub.subscribe(
        Filter::all(),
        SubOpts {
            capacity: Some(4),
            ..SubOpts::default()
        },
    );

    for i in 0..50 {
        hub.publish(env("noisy/tick", false, json!({"i": i})))
            .await
            .unwrap();
    }
    let published_durable = 50usize;
    for i in 0..published_durable {
        hub.publish(env("keep/it", true, json!({"i": i})))
            .await
            .unwrap();
    }

    let batch = sub.recv().await.expect("a batch");
    let durable_seen = batch.iter().filter(|m| m.envelope.durable).count();
    let non_durable_seen = batch.iter().filter(|m| !m.envelope.durable).count();
    assert_eq!(
        durable_seen, published_durable,
        "a durable envelope was dropped from a full ring"
    );
    assert!(
        non_durable_seen <= 4,
        "the ring kept more non-durable envelopes than its capacity: {non_durable_seen}"
    );
    assert!(sub.ring().dropped() > 0, "nothing was reported dropped");

    // since_offset replay independently proves durable delivery is never lost
    // even when the live ring for a subscriber was never read at all.
    let replay = hub.subscribe(
        Filter {
            topics: vec!["keep/**".to_string()],
            ..Filter::default()
        },
        SubOpts {
            since_offset: Some(0),
            ..SubOpts::default()
        },
    );
    let replayed = replay.recv().await.expect("replay batch");
    assert_eq!(replayed.len(), published_durable);
}

/// A subscriber that never calls `recv()` must not stall the publisher or any
/// other subscriber: the hub's per-subscriber ring is independent and publish
/// is lock-free fan-out, not a broadcast that waits on the slowest reader.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_that_never_reads_does_not_stall_the_publisher_or_others() {
    let hub = Hub::new();
    let _never_read = hub.subscribe(
        Filter::all(),
        SubOpts {
            capacity: Some(2),
            ..SubOpts::default()
        },
    );
    let attentive = hub.subscribe(
        Filter::all(),
        SubOpts {
            capacity: Some(20_000),
            ..SubOpts::default()
        },
    );

    let start = std::time::Instant::now();
    for i in 0..20_000 {
        hub.emit(env("noisy/flood", false, json!({"i": i})));
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "publish stalled behind an unread subscriber: {elapsed:?}"
    );

    let mut seen = 0usize;
    while seen < 20_000 {
        match tokio::time::timeout(Duration::from_secs(5), attentive.recv()).await {
            Ok(Some(batch)) => seen += batch.len(),
            _ => break,
        }
    }
    assert_eq!(seen, 20_000, "an attentive subscriber lost envelopes");
}

/// RFC budget: 100k msg/s fan-out, no loss for a subscriber whose ring is
/// sized to hold them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hundred_k_burst_is_not_lost_by_a_sized_subscriber() {
    let hub = Hub::new();
    let n = 100_000usize;
    let sub = hub.subscribe(
        Filter::all(),
        SubOpts {
            capacity: Some(n * 2),
            ..SubOpts::default()
        },
    );
    for i in 0..n {
        hub.emit(env("burst/x", false, json!({"i": i})));
    }
    let mut seen = 0usize;
    while seen < n {
        match tokio::time::timeout(Duration::from_secs(10), sub.recv()).await {
            Ok(Some(batch)) => seen += batch.len(),
            _ => break,
        }
    }
    assert_eq!(seen, n, "the sized ring must not drop a 100k burst");
}

/// `latest_only`: rapid updates to the same compaction key collapse to the
/// last one, per key, not globally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latest_only_survives_rapid_same_key_updates_and_keeps_other_keys() {
    let hub = Hub::new();
    let sub = hub.subscribe(
        Filter::all(),
        SubOpts {
            latest_only: true,
            ..SubOpts::default()
        },
    );
    for i in 0..500 {
        let mut e = env("status/cpu", false, json!({"pct": i}));
        e.tags.insert("key".to_string(), "host-a".to_string());
        hub.emit(e);
    }
    let mut other = env("status/cpu", false, json!({"pct": -1}));
    other.tags.insert("key".to_string(), "host-b".to_string());
    hub.emit(other);

    tokio::time::sleep(Duration::from_millis(20)).await;
    let batch = sub.recv().await.expect("batch");
    assert_eq!(batch.len(), 2, "one row per compaction key, not per update");
    let host_a = batch
        .iter()
        .find(|m| m.envelope.tags.get("key").map(String::as_str) == Some("host-a"))
        .expect("host-a row");
    assert_eq!(
        host_a.envelope.payload["pct"], 499,
        "latest_only kept a stale value instead of the last write"
    );
}

/// `coalesce_ms`: a burst inside the window arrives as one `recv()`, and the
/// batch must still contain the true final state of every key in the burst —
/// coalescing batches delivery, it must never drop the tail of the burst.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coalesce_batches_a_burst_without_dropping_the_final_state() {
    let hub = Hub::new();
    let sub = hub.subscribe(
        Filter::all(),
        SubOpts {
            coalesce_ms: Some(50),
            capacity: Some(1000),
            ..SubOpts::default()
        },
    );
    for i in 0..300 {
        hub.emit(env("coalesce/x", false, json!({"i": i})));
    }
    let batch = sub.recv().await.expect("coalesced batch");
    assert_eq!(batch.len(), 300, "coalesce must not drop envelopes");
    let last = batch.last().expect("a last envelope");
    assert_eq!(
        last.envelope.payload["i"], 299,
        "the final state of the burst went missing"
    );
}

/// RFC section 2: `ttl_s` is documented as expiring an envelope for delivery
/// and storage. This asserts the documented contract: a durable envelope
/// published with a 1s ttl must not be handed to a subscriber that shows up
/// after it has expired, even though the log still holds it before that
/// offset window. If this fails, ttl_s is accepted on the wire but never
/// enforced anywhere in the hub/ring/durable path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ttl_s_expires_an_envelope_for_a_late_subscriber() {
    let log = Arc::new(MemLog::default());
    let hub = Hub::with_durable(log.clone());

    let mut expiring = env("ttl/gone", true, json!({"v": 1}));
    expiring.ttl_s = Some(1);
    hub.publish(expiring).await.expect("publish");

    tokio::time::sleep(Duration::from_millis(1200)).await;

    let sub = hub.subscribe(
        Filter {
            topics: vec!["ttl/**".to_string()],
            ..Filter::default()
        },
        SubOpts {
            since_offset: Some(0),
            ..SubOpts::default()
        },
    );
    let outcome = tokio::time::timeout(Duration::from_millis(300), sub.recv()).await;
    let still_there = matches!(outcome, Ok(Some(batch)) if !batch.is_empty());
    assert!(
        !still_there,
        "ttl_s=1 did not expire the envelope: a late replay still delivered it \
         (ttl_s is stored on the envelope but rs/bus never checks it, see \
         rs/bus/src/hub.rs and rs/bus/src/ring.rs)"
    );
}

/// Topic-glob trickery cannot be used to escape a caller's env: `Filter.env`
/// is matched independently of the topic pattern, so no glob shape (leading
/// slash, `..`-looking segments, empty segments, brace lists) makes a
/// same-topic envelope from another env match. This is a property of the
/// filter itself; RFC 8d.2 pins `Filter.env` at the front door before the
/// filter ever sees a request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn topic_glob_tricks_never_widen_past_the_pinned_env() {
    let hub = Hub::new();
    let tricky_patterns = [
        "**",
        "/**",
        "../**",
        "b/*",
        "",
        "{a,b}/*",
        "session/**",
        "*/x",
    ];
    for pattern in tricky_patterns {
        let sub = hub.subscribe(
            Filter {
                topics: if pattern.is_empty() {
                    vec![]
                } else {
                    vec![pattern.to_string()]
                },
                env: Some("a".to_string()),
                ..Filter::default()
            },
            SubOpts::default(),
        );
        let mut foreign = env("b/x", false, json!({"leak": true}));
        foreign.env = Some("b".to_string());
        hub.emit(foreign);
        let outcome = tokio::time::timeout(Duration::from_millis(80), sub.recv()).await;
        assert!(
            outcome.is_err(),
            "pattern {pattern:?} let env b leak into a subscriber pinned to env a"
        );
    }
}

/// A brace pattern is not expanded (the glob only understands `*` and `**`
/// segments); it is matched as a literal segment, so it simply never matches
/// anything real. This nails down that behaviour so a future change to
/// `glob()` cannot silently start treating `{a,b}` as alternation without a
/// test noticing.
#[test]
fn brace_patterns_are_literal_not_alternation() {
    assert!(!glob("{a,b}/x", "a/x"));
    assert!(!glob("{a,b}/x", "b/x"));
    assert!(glob("{a,b}/x", "{a,b}/x"));
}

/// Path-traversal-looking segments are just literal topic segments to the
/// glob matcher — there is no filesystem underneath `topic` to traverse.
#[test]
fn dot_dot_segments_are_literal() {
    assert!(!glob("../secret", "a/secret"));
    assert!(glob("../secret", "../secret"));
    assert!(!glob("a/../b", "b"));
}

/// An empty prefix/pattern list means "no constraint" (RFC section 3), not
/// "match nothing" — that is `Filter::all()`'s topics being empty, already
/// covered by `filter.rs`. Here we confirm an explicit empty-string pattern
/// behaves as a literal empty topic, not a wildcard.
#[test]
fn empty_string_pattern_is_literal_not_wildcard() {
    assert!(!glob("", "a/b"));
    assert!(glob("", ""));
}

/// A malformed/huge `coalesce_ms` must not panic the hub when constructing a
/// subscription (`Duration::from_millis` on a huge u64 is valid but a
/// subscriber that asks for it will simply never see a coalesced batch inside
/// any sane test timeout — this only proves subscribe() itself is safe).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absurd_coalesce_ms_does_not_panic_subscribe() {
    let hub = Hub::new();
    let sub = hub.subscribe(
        Filter::all(),
        SubOpts {
            coalesce_ms: Some(u64::MAX),
            ..SubOpts::default()
        },
    );
    hub.emit(env("x/y", false, json!({})));
    let outcome = tokio::time::timeout(Duration::from_millis(200), sub.recv()).await;
    assert!(
        outcome.is_err(),
        "an absurd coalesce_ms should not resolve almost instantly"
    );
}

/// Zero capacity is clamped to at least 1 rather than making every push a
/// drop with nothing ever delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_capacity_ring_still_delivers_one_at_a_time() {
    let hub = Hub::new();
    let sub = hub.subscribe(
        Filter::all(),
        SubOpts {
            capacity: Some(0),
            ..SubOpts::default()
        },
    );
    hub.emit(env("z/1", false, json!({"n": 1})));
    let batch = tokio::time::timeout(Duration::from_secs(2), sub.recv())
        .await
        .expect("recv timed out")
        .expect("a batch");
    assert!(!batch.is_empty(), "zero capacity delivered nothing at all");
}

/// A concurrent stress mix of publishers and subscribers must not panic the
/// hub and must not lose durable envelopes that a since_offset replay from 0
/// picks up after everything settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_publish_and_subscribe_churn_does_not_lose_durable_replay() {
    let log = Arc::new(MemLog::default());
    let hub = Hub::with_durable(log.clone());
    let published = Arc::new(AtomicU64::new(0));

    let mut tasks = Vec::new();
    for worker in 0..8 {
        let hub = hub.clone();
        let published = published.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..200 {
                hub.publish(env("churn/x", true, json!({"worker": worker, "i": i})))
                    .await
                    .expect("publish");
                published.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for worker in 0..4 {
        let hub = hub.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..20 {
                let sub = hub.subscribe(Filter::all(), SubOpts::default());
                let _ = tokio::time::timeout(Duration::from_millis(5), sub.recv()).await;
                let _ = worker;
            }
        }));
    }
    for task in tasks {
        task.await.expect("task panicked");
    }

    let expect = published.load(Ordering::Relaxed);
    let replay = hub.subscribe(
        Filter {
            topics: vec!["churn/**".to_string()],
            ..Filter::default()
        },
        SubOpts {
            since_offset: Some(0),
            capacity: Some(expect as usize + 10),
            ..SubOpts::default()
        },
    );
    let mut seen = 0usize;
    while seen < expect as usize {
        match tokio::time::timeout(Duration::from_secs(5), replay.recv()).await {
            Ok(Some(batch)) => seen += batch.len(),
            _ => break,
        }
    }
    assert_eq!(
        seen, expect as usize,
        "since_offset replay lost durable envelopes under concurrent churn"
    );
}
