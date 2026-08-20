mod gate;

use gate::{fixture, skip};
use serde_json::json;
use std::time::{Duration, Instant};
use tenon_harness::fake::{self, Fake, Say};
use tenon_storage::{Aggregate, QueryFilter, Source, Store};

const NAME: &str = "query-gate";
const MARKER: &str = "tenon-marker-9z7";

fn config() -> String {
    "sandbox: oci\nworker:\n  pull_interval_ms: 60000\n".to_string()
}

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 4\napproval: deny\n"
    )
}

/// The hot query layer end to end: a real turn writes a session log, then
/// `query.text` finds the tool call by keyword with a snippet and the source
/// event ref, `query.scan` aggregates episode cost and tool-result status, and
/// the 8d.2 authorizer refuses a scoped caller that names another env.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn query_text_and_scan_over_the_session_log_respect_env_scope() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let server: Fake = fake::spawn(vec![
        Say::Tool("bash".to_string(), json!({"cmd": format!("echo {MARKER}")})),
        Say::Text(format!("the marker was {MARKER}")),
    ])
    .await
    .expect("fake model");
    let fixture = fixture(NAME, release, &config(), &harness(&server.base_url));
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    let (ok, out, err) = fixture.run(&["run", "find the marker", "--timeout", "120"]);
    assert!(ok, "tenon run failed: {out}{err}\n{}", fixture.log());

    // a. text search finds the tool call/result by keyword, ranked, with a
    // highlighted snippet and the source event ref.
    let hits = fixture
        .rpc(
            "query.text",
            json!({"env": "root", "q": MARKER, "topk": 10}),
        )
        .await
        .expect("query.text")["hits"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!hits.is_empty(), "no text hit for {MARKER}");
    assert!(
        hits.iter()
            .any(|hit| hit["snippet"].as_str().unwrap_or_default().contains(MARKER)),
        "no snippet carried the marker: {hits:?}"
    );
    assert!(
        hits[0]["ref"].as_i64().unwrap_or(0) > 0,
        "the hit has no source event ref: {}",
        hits[0]
    );

    // b. scan aggregates: episode cost summed, and tool-result status counted.
    let cost = fixture
        .rpc(
            "query.scan",
            json!({"env": "root", "source": "episodes",
                   "aggregate": {"op": "sum", "field": "cost"}}),
        )
        .await
        .expect("query.scan cost");
    let summed = cost["groups"][0]["value"].as_i64().unwrap_or(0);
    assert_eq!(summed, 36, "two steps of cost 18 sum to 36: {cost}");

    let status = fixture
        .rpc(
            "query.scan",
            json!({"env": "root", "source": "tool_results",
                   "aggregate": {"op": "count", "group_by": "status"}}),
        )
        .await
        .expect("query.scan status")["groups"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let okays = status
        .iter()
        .find(|group| group["key"] == json!("ok"))
        .and_then(|group| group["value"].as_i64())
        .unwrap_or(0);
    assert_eq!(okays, 1, "one ok tool result: {status:?}");

    // c. env-scope: a caller bound to root cannot query another env.
    let token = std::fs::read_to_string(fixture.home.join("run/rt-root.token"))
        .expect("runtime token")
        .trim()
        .to_string();
    let mut scoped = fixture.client().await;
    scoped
        .call("auth.scope", json!({"env": "root", "token": token}))
        .await
        .expect("scope to root");
    let own = scoped
        .call("query.text", json!({"env": "root", "q": MARKER}))
        .await;
    assert!(
        own.is_ok(),
        "a scoped caller cannot read its own env: {own:?}"
    );
    let cross = scoped
        .call("query.text", json!({"env": "guardian", "q": MARKER}))
        .await;
    assert!(cross.is_err(), "a scoped caller queried another env");
    assert!(cross.unwrap_err().to_string().contains("cross_env_denied"));
}

fn temp_store(tag: &str) -> (std::path::PathBuf, Store) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("tenon-query-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let store = Store::open(&dir.join("state.sqlite")).expect("open store");
    (dir, store)
}

fn seed(store: &Store, kind: &str, session: &str, text: &str) {
    store
        .append(
            kind,
            Some("root"),
            &json!({"session": session, "text": text}),
        )
        .expect("append");
}

/// A version bump drops and rebuilds the derived index from the log, so the
/// same query reproduces its results — including events appended after the
/// first index was built (RFC section 5: log = truth, indexes disposable).
#[test]
fn index_rebuild_after_a_version_bump_reproduces_results_from_the_log() {
    let (dir, store) = temp_store("rebuild");
    seed(&store, "user/message", "s1", "alpha needle beta");
    seed(&store, "assistant/message", "s1", "unrelated");
    let first = store
        .query_text("needle", &QueryFilter::default(), 10)
        .expect("first query");
    assert_eq!(first.len(), 1, "the needle is indexed once: {first:?}");

    seed(&store, "tool/result", "s1", "another needle here");
    store.query_reset_index().expect("reset");
    let rebuilt = store
        .query_text("needle", &QueryFilter::default(), 10)
        .expect("rebuilt query");
    assert_eq!(
        rebuilt.len(),
        2,
        "the rebuild reproduced both log rows: {rebuilt:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn percentile(sorted: &[u128], pct: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() as f64 - 1.0) * pct).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Hot-window budgets at ~1M events (RFC section 9 gate). Ignored by default;
/// run with `cargo test -p tenon-cli --release -- --ignored perf_1m`.
#[test]
#[ignore]
fn perf_1m_events_text_under_10ms_scan_under_100ms() {
    let (dir, store) = temp_store("perf");
    let total: i64 = 1_000_000;
    let build = Instant::now();
    for id in 0..total {
        let session = format!("s{}", id % 200);
        let text = match id {
            777_777 => format!("rare {MARKER} token"),
            _ => "ordinary chatter about the workspace".to_string(),
        };
        let kind = match id % 3 {
            0 => "user/message",
            1 => "assistant/message",
            _ => "tool/result",
        };
        store
            .append(
                kind,
                Some("root"),
                &json!({"session": session, "text": text}),
            )
            .expect("append");
    }
    let index = Instant::now();
    store.query_ensure_index().expect("index");
    println!(
        "perf: inserted {total} events in {:?}, built index in {:?}",
        index.duration_since(build),
        index.elapsed()
    );

    let filter = QueryFilter::default();
    let _ = store.query_text(MARKER, &filter, 10).expect("warm text");
    let _ = store
        .query_scan(
            Source::Events,
            &filter,
            Some(Aggregate {
                op: "count".to_string(),
                field: None,
                group_by: Some("kind".to_string()),
            }),
            10,
        )
        .expect("warm scan");

    let mut text_us = Vec::new();
    let mut scan_us = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        let hits = store.query_text(MARKER, &filter, 10).expect("text");
        text_us.push(t.elapsed().as_micros());
        assert_eq!(hits.len(), 1, "the rare marker is found exactly once");

        let s = Instant::now();
        store
            .query_scan(
                Source::Events,
                &filter,
                Some(Aggregate {
                    op: "count".to_string(),
                    field: None,
                    group_by: Some("kind".to_string()),
                }),
                10,
            )
            .expect("scan");
        scan_us.push(s.elapsed().as_micros());
    }
    text_us.sort_unstable();
    scan_us.sort_unstable();
    let (t50, t99) = (percentile(&text_us, 0.50), percentile(&text_us, 0.99));
    let (s50, s99) = (percentile(&scan_us, 0.50), percentile(&scan_us, 0.99));
    println!("perf: text p50={t50}us p99={t99}us  scan p50={s50}us p99={s99}us");
    assert!(t99 < 10_000, "text p99 {t99}us exceeds 10ms");
    assert!(s99 < 100_000, "scan p99 {s99}us exceeds 100ms");
    let _ = std::fs::remove_dir_all(&dir);
}
