mod gate;

use gate::{skip, skip_release, Fixture, Spec, BIN};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tenon_base::client::Client;
use tenon_harness::fake::{self, Fake, Say};

const NAME: &str = "trigger-gate";

/// Read `t:"ev"` frames until one carries `topic`, or the deadline passes.
async fn next_topic(client: &mut Client, topic: &str, limit: Duration) -> Option<Value> {
    let deadline = Instant::now() + limit;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        match tokio::time::timeout(left, client.next_ev()).await {
            Ok(Ok(Some(event))) if event["topic"] == json!(topic) => return Some(event),
            Ok(Ok(Some(_))) => continue,
            _ => return None,
        }
    }
}

/// Count the `topic` envelopes that arrive inside `window` (the loop guard test:
/// a bounded burst, not a runaway).
async fn count_topic(client: &mut Client, topic: &str, window: Duration) -> usize {
    let deadline = Instant::now() + window;
    let mut count = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return count;
        }
        match tokio::time::timeout(left, client.next_ev()).await {
            Ok(Ok(Some(event))) if event["topic"] == json!(topic) => count += 1,
            Ok(Ok(Some(_))) => continue,
            _ => return count,
        }
    }
}

/// A one-shot local HTTP sink: fails the first request with 500, then answers
/// 200, and counts every request. Its `hits` is the retry evidence.
fn http_sink() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sink");
    let addr = listener.local_addr().expect("addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(4) {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let status = if n == 0 { "500 Error" } else { "200 OK" };
            let _ = stream.write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
        }
    });
    (format!("http://{addr}/hook"), hits)
}

async fn publish(client: &mut Client, topic: &str, env: &str, payload: Value) {
    client
        .call(
            "bus.publish",
            json!({"envelope": {"topic": topic, "env": env, "payload": payload}}),
        )
        .await
        .expect("bus.publish");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn triggers_relay_retry_bound_a_loop_and_stay_env_scoped() {
    let Some(release) = skip_release(NAME) else {
        return;
    };
    let fixture = Fixture::open(
        BIN,
        release,
        Spec {
            name: NAME,
            config: Some("sandbox: none\n"),
            ..Spec::default()
        },
    );
    fixture.start();
    fixture.registered("root", Duration::from_secs(60)).await;

    // 1. a publish trigger relays an event with templated payload.
    let mut sub = fixture.client().await;
    sub.call("bus.subscribe", json!({"topics": ["app/**"]}))
        .await
        .expect("subscribe");
    let mut ctl = fixture.client().await;
    ctl.call(
        "trigger.set",
        json!({
            "trigger_id": "relay",
            "filter": {"topics": ["app/ping"]},
            "action": {"type": "publish", "topic": "app/relayed",
                       "payload_template": {"seen": "${payload.n}"}},
        }),
    )
    .await
    .expect("trigger.set relay");
    publish(&mut ctl, "app/ping", "root", json!({"n": 7})).await;
    let relayed = next_topic(&mut sub, "app/relayed", Duration::from_secs(5))
        .await
        .expect("app/relayed did not fire");
    assert_eq!(relayed["payload"]["seen"], json!("7"), "{relayed}");

    // 2. an http_post trigger retries after a first-request failure.
    let (url, hits) = http_sink();
    ctl.call(
        "trigger.set",
        json!({
            "trigger_id": "webhook",
            "filter": {"topics": ["app/hook"]},
            "action": {"type": "http_post", "url": url},
        }),
    )
    .await
    .expect("trigger.set http_post");
    publish(&mut ctl, "app/hook", "root", json!({"x": 1})).await;
    let deadline = Instant::now() + Duration::from_secs(6);
    while hits.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        hits.load(Ordering::SeqCst) >= 2,
        "http_post did not retry after the first failure"
    );

    // 3. the hop cap stops a publish -> trigger -> publish loop.
    let mut loops = fixture.client().await;
    loops
        .call("bus.subscribe", json!({"topics": ["loop/x"]}))
        .await
        .expect("subscribe loop");
    ctl.call(
        "trigger.set",
        json!({
            "trigger_id": "cycle",
            "filter": {"topics": ["loop/x"]},
            "action": {"type": "publish", "topic": "loop/x"},
        }),
    )
    .await
    .expect("trigger.set cycle");
    publish(&mut ctl, "loop/x", "root", json!({})).await;
    let count = count_topic(&mut loops, "loop/x", Duration::from_secs(2)).await;
    assert!(
        (1..=8).contains(&count),
        "hop cap did not bound the loop: saw {count}"
    );

    // 4. a trigger only fires on its own env's envelopes (RFC 8d.2).
    let mut scope = fixture.client().await;
    scope
        .call("bus.subscribe", json!({"topics": ["scope/relayed"]}))
        .await
        .expect("subscribe scope");
    ctl.call(
        "trigger.set",
        json!({
            "trigger_id": "scoped",
            "filter": {"topics": ["scope/ping"]},
            "action": {"type": "publish", "topic": "scope/relayed"},
        }),
    )
    .await
    .expect("trigger.set scoped");
    publish(&mut ctl, "scope/ping", "other", json!({})).await;
    assert!(
        next_topic(&mut scope, "scope/relayed", Duration::from_secs(1))
            .await
            .is_none(),
        "a foreign env's envelope must not fire a root trigger"
    );
    publish(&mut ctl, "scope/ping", "root", json!({})).await;
    assert!(
        next_topic(&mut scope, "scope/relayed", Duration::from_secs(5))
            .await
            .is_some(),
        "the trigger's own env must fire it"
    );

    // trigger.list shows the durable rules; del removes one.
    let listed = ctl.call("trigger.list", json!({})).await.expect("list");
    assert!(listed["count"].as_i64().unwrap_or(0) >= 4, "{listed}");
    ctl.call("trigger.del", json!({"trigger_id": "relay"}))
        .await
        .expect("del");

    fixture.run(&["stop"]);
}

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 4\napproval: deny\n"
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prompt_trigger_wakes_an_agent_turn() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let server: Fake = fake::spawn(vec![Say::Text("woken by trigger".to_string())])
        .await
        .expect("fake model");
    let fixture = gate::fixture(NAME, release, "sandbox: oci\n", &harness(&server.base_url));
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;
    fixture.worker_ready("root", Duration::from_secs(120)).await;

    let mut ctl = fixture.client().await;
    ctl.call(
        "trigger.set",
        json!({
            "trigger_id": "wake",
            "filter": {"topics": ["wake/now"]},
            "action": {"type": "prompt", "text_template": "say ${payload.what}"},
        }),
    )
    .await
    .expect("trigger.set prompt");
    publish(&mut ctl, "wake/now", "root", json!({"what": "hello"})).await;

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut woke = false;
    while Instant::now() < deadline {
        let ended = fixture.of_kind("turn/end").await;
        if !ended.is_empty() {
            woke = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        woke,
        "the prompt trigger never woke a turn\n{}",
        fixture.log()
    );
    fixture.run(&["stop"]);
}
