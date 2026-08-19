mod gate;

use gate::{kill_alive, skip_release, wait_gone, Fixture, Spec, BIN};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tenon_base::client::Client;

/// No container: the fabric is base's, and a session never runs in these tests,
/// so the harness only needs a config that boots.
const CONFIG: &str = "sandbox: none\n";
const HARNESS: &str = "llm:\n  provider: openai\n  base_url: http://127.0.0.1:1\n  \
     model: fake-model\n  api_key_env: TENON_TEST_NO_KEY\nmax_steps: 2\napproval: deny\n";

fn fixture(name: &str) -> Option<Fixture> {
    let release = skip_release(name)?;
    Some(Fixture::open(
        BIN,
        release,
        Spec {
            name,
            config: Some(CONFIG),
            harness: Some(HARNESS),
            reap_pids: true,
            lock: true,
            ..Spec::default()
        },
    ))
}

async fn client(fixture: &Fixture) -> Client {
    Client::connect(&fixture.sock()).await.expect("connect")
}

async fn subscribe(fixture: &Fixture, body: Value) -> Client {
    let mut client = client(fixture).await;
    tokio::time::timeout(Duration::from_secs(10), client.call("bus.subscribe", body))
        .await
        .expect("subscribe timed out")
        .expect("subscribe");
    client
}

async fn next_ev(client: &mut Client, limit: Duration) -> Value {
    tokio::time::timeout(limit, client.next_ev())
        .await
        .expect("ev timed out")
        .expect("read")
        .expect("an ev frame")
}

async fn publish(client: &mut Client, topic: &str, payload: Value) -> i64 {
    let answer = client
        .call(
            "bus.publish",
            json!({"envelope": {"topic": topic, "durable": true, "payload": payload}}),
        )
        .await
        .expect("publish");
    answer["offset"].as_i64().unwrap_or(0)
}

async fn wait_ready(fixture: &Fixture) {
    let ok = fixture
        .await_status(Duration::from_secs(120), |status| {
            status["nodes"]
                .as_array()
                .map(|n| n.len() >= 2)
                .unwrap_or(false)
        })
        .await;
    assert!(ok, "base never came up\n{}", fixture.log());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_subscribe_filter_and_since_offset_replay() {
    let Some(fixture) = fixture("bus-roundtrip") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut sub = subscribe(&fixture, json!({"topics": ["test/**"]})).await;
    let mut publisher = client(&fixture).await;

    let off_a = publish(&mut publisher, "test/a", json!({"n": 1})).await;
    let ev = next_ev(&mut sub, Duration::from_secs(5)).await;
    assert_eq!(ev["topic"], "test/a");
    assert_eq!(ev["payload"]["n"], 1);
    assert_eq!(ev["offset"].as_i64(), Some(off_a));

    // a topic outside the filter is never delivered: the next ev is test/b
    publish(&mut publisher, "other/x", json!({"skip": true})).await;
    let off_b = publish(&mut publisher, "test/b", json!({"n": 2})).await;
    let ev = next_ev(&mut sub, Duration::from_secs(5)).await;
    assert_eq!(
        ev["topic"], "test/b",
        "the other/x topic leaked past the filter"
    );
    assert_eq!(ev["offset"].as_i64(), Some(off_b));

    // a simulated reconnect: drop the subscriber, publish, resubscribe with the
    // last offset seen, and the missed envelope replays from the log
    drop(sub);
    let off_c = publish(&mut publisher, "test/c", json!({"n": 3})).await;
    let mut reconnect = subscribe(
        &fixture,
        json!({"topics": ["test/**"], "since_offset": off_b}),
    )
    .await;
    let ev = next_ev(&mut reconnect, Duration::from_secs(5)).await;
    assert_eq!(ev["topic"], "test/c");
    assert_eq!(ev["offset"].as_i64(), Some(off_c));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_durable_envelope_survives_kill_9_and_replays_from_the_log() {
    let Some(fixture) = fixture("bus-durable") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut publisher = client(&fixture).await;
    let offset = publish(&mut publisher, "keep/1", json!({"kept": true})).await;
    assert!(offset > 0, "a durable publish must get a log offset");
    drop(publisher);

    restart(&fixture);

    let mut sub = subscribe(&fixture, json!({"topics": ["keep/**"], "since_offset": 0})).await;
    let ev = next_ev(&mut sub, Duration::from_secs(5)).await;
    assert_eq!(ev["topic"], "keep/1");
    assert_eq!(ev["payload"]["kept"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kv_get_set_cas_incr_lease_and_watch() {
    let Some(fixture) = fixture("bus-kv") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let mut c = client(&fixture).await;

    c.call(
        "kv.set",
        json!({"env": "root", "key": "/a", "value": "1", "durable": true}),
    )
    .await
    .expect("set");
    let got = c
        .call("kv.get", json!({"env": "root", "key": "/a"}))
        .await
        .expect("get");
    assert_eq!(got["found"], true);
    assert_eq!(got["value"], "1");

    let incr = c
        .call(
            "kv.incr",
            json!({"env": "root", "key": "/a", "delta": 5, "durable": true}),
        )
        .await
        .expect("incr");
    assert_eq!(incr["value"], 6);

    c.call(
        "kv.cas",
        json!({"env": "root", "key": "/a", "expect": "6", "value": "7", "durable": true}),
    )
    .await
    .expect("cas ok");
    let bad = c
        .call(
            "kv.cas",
            json!({"env": "root", "key": "/a", "expect": "6", "value": "9"}),
        )
        .await;
    assert!(bad.is_err(), "cas with a stale expect must fail");

    // watch sees a live set
    let mut watch = client(&fixture).await;
    watch
        .call("kv.watch", json!({"env": "root", "prefix": "/w/"}))
        .await
        .expect("watch");
    c.call(
        "kv.set",
        json!({"env": "root", "key": "/w/x", "value": "hi", "durable": true}),
    )
    .await
    .expect("set watched");
    let ev = next_ev(&mut watch, Duration::from_secs(5)).await;
    assert_eq!(ev["payload"]["op"], "set");
    assert_eq!(ev["payload"]["key"], "/w/x");

    // a lease-bound key is deleted when the lease expires
    let lease = c
        .call("kv.lease", json!({"env": "root", "ttl_ms": 200}))
        .await
        .expect("lease");
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();
    c.call(
        "kv.set",
        json!({"env": "root", "key": "/leased", "value": "x", "durable": true, "lease_id": lease_id}),
    )
    .await
    .expect("leased set");
    let mut gone = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let got = c
            .call("kv.get", json!({"env": "root", "key": "/leased"}))
            .await
            .expect("get leased");
        if got["found"] == json!(false) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(gone, "the lease never expired its key");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_put_get_open_and_dedup() {
    let Some(fixture) = fixture("bus-blob") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let mut c = client(&fixture).await;

    let data = b64("hello");
    let put = c
        .call("blob.put", json!({"env": "root", "data": data}))
        .await
        .expect("put");
    let hash = put["hash"].as_str().unwrap().to_string();
    let again = c
        .call("blob.put", json!({"env": "root", "data": b64("hello")}))
        .await
        .expect("put again");
    assert_eq!(again["hash"], put["hash"], "identical bytes must dedup");

    let got = c
        .call("blob.get", json!({"env": "root", "hash": hash}))
        .await
        .expect("get");
    assert_eq!(unb64(got["data"].as_str().unwrap()), b"hello");
    let window = c
        .call(
            "blob.open",
            json!({"env": "root", "hash": hash, "offset": 1, "len": 3}),
        )
        .await
        .expect("open");
    assert_eq!(unb64(window["data"].as_str().unwrap()), b"ell");
    let stat = c
        .call("blob.stat", json!({"env": "root", "hash": hash}))
        .await
        .expect("stat");
    assert_eq!(stat["size"], 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_timer_fires_and_a_persisted_timer_fires_after_a_restart() {
    let Some(fixture) = fixture("bus-timer") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    // an after_ms timer fires on schedule
    let mut sub = subscribe(&fixture, json!({"topics": ["fired/**"]})).await;
    let mut c = client(&fixture).await;
    c.call(
        "timer.set",
        json!({"env": "root", "topic": "fired/once", "after_ms": 200, "payload": {"k": 1}}),
    )
    .await
    .expect("timer.set");
    let ev = next_ev(&mut sub, Duration::from_secs(5)).await;
    assert_eq!(ev["topic"], "fired/once");
    assert_eq!(ev["payload"]["k"], 1);
    drop(sub);

    // a timer that has not fired yet survives a restart and fires afterwards.
    // `timer_id` names it; the frame's own `id` is the correlation key.
    c.call(
        "timer.set",
        json!({"env": "root", "timer_id": "survivor", "topic": "fired/later", "after_ms": 4000}),
    )
    .await
    .expect("timer.set survivor");
    drop(c);

    restart(&fixture);

    // since_offset replays the fire even if it landed before we resubscribed;
    // the exact topic keeps the earlier fired/once out of the way.
    let mut sub = subscribe(
        &fixture,
        json!({"topics": ["fired/later"], "since_offset": 0}),
    )
    .await;
    let ev = next_ev(&mut sub, Duration::from_secs(15)).await;
    assert_eq!(
        ev["topic"], "fired/later",
        "the persisted timer never fired"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn env_scoping_denies_cross_env_reads_and_subscribes() {
    let Some(fixture) = fixture("bus-scope") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let token = std::fs::read_to_string(fixture.home.join("run/rt-root.token"))
        .expect("runtime token")
        .trim()
        .to_string();

    let mut c = client(&fixture).await;
    // a wrong token is refused
    assert!(c
        .call("auth.scope", json!({"env": "root", "token": "nope"}))
        .await
        .is_err());
    // the right token binds the connection to root
    c.call("auth.scope", json!({"env": "root", "token": token}))
        .await
        .expect("scope");

    // inside its env everything works
    c.call(
        "kv.set",
        json!({"env": "root", "key": "/s", "value": "1", "durable": true}),
    )
    .await
    .expect("own env set");
    // naming another env is denied on every facade
    let kv = c.call("kv.get", json!({"env": "other", "key": "/s"})).await;
    assert!(kv.is_err(), "a scoped caller read another env's kv");
    assert!(kv.unwrap_err().to_string().contains("cross_env_denied"));
    let sub = c
        .call("bus.subscribe", json!({"env": "other", "topics": ["**"]}))
        .await;
    assert!(sub.is_err(), "a scoped caller subscribed to another env");
}

/// Section 4 budgets, in-process against the hub itself (no socket): publish ->
/// subscriber latency p99 and throughput. Ignored by default; run with
/// `cargo test -p tenon-cli --release -- --ignored bench_hub`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore]
async fn bench_hub_latency_and_throughput() {
    use tenon_bus::{Envelope, Filter, Hub, Level, SubOpts};

    let hub = Hub::new();

    // throughput: one producer, one consumer, 100k non-durable envelopes. A
    // ring big enough that nothing drops, so the consumer sees every one.
    let n = 100_000usize;
    let sub = hub.subscribe(
        Filter::all(),
        SubOpts {
            capacity: Some(n * 2),
            ..SubOpts::default()
        },
    );
    let consumer = tokio::spawn(async move {
        let mut seen = 0usize;
        while seen < n {
            match sub.recv().await {
                Some(batch) => seen += batch.len(),
                None => break,
            }
        }
        seen
    });
    let start = Instant::now();
    for i in 0..n {
        hub.emit(Envelope::new(
            "bench/throughput",
            Level::Info,
            json!({"i": i}),
        ));
    }
    let seen = consumer.await.unwrap();
    let elapsed = start.elapsed();
    let rate = seen as f64 / elapsed.as_secs_f64();
    println!("throughput: {seen} envelopes in {elapsed:?} = {rate:.0} msg/s");
    assert_eq!(seen, n, "the sized ring must not drop");
    assert!(rate > 200_000.0, "throughput {rate:.0} msg/s below floor");

    // latency: publish one, await it, per-message wake time
    let sub = hub.subscribe(Filter::all(), SubOpts::default());
    let mut samples = Vec::new();
    for i in 0..2000 {
        let at = Instant::now();
        hub.emit(Envelope::new("bench/latency", Level::Info, json!({"i": i})));
        let _ = sub.recv().await;
        samples.push(at.elapsed());
    }
    samples.sort();
    let p99 = samples[samples.len() * 99 / 100];
    let p50 = samples[samples.len() / 2];
    println!("latency: p50 {p50:?} p99 {p99:?}");
    assert!(p99 < Duration::from_millis(10), "p99 {p99:?} over budget");
}

fn b64(text: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

fn unb64(text: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap()
}

/// Hard-kill base and wait for its whole process group to die, then start a
/// fresh one. `kill -9` leaves the stale sock and ready file, which `start`
/// clears itself; the wait is on the processes, not the files.
fn restart(fixture: &Fixture) {
    let base = fixture.base_pid();
    let mut targets = fixture.node_pids();
    targets.push(base);
    kill_alive(base, "-9");
    wait_gone(&targets, Duration::from_secs(30));
    let (ok, text) = fixture.run_text(&["start"]);
    assert!(ok, "restart failed: {text}\n{}", fixture.log());
    // the fresh base owns a new socket; give the client something to dial
    assert!(
        wait_for(
            &fixture.home.join("run/base.ready"),
            true,
            Duration::from_secs(30)
        ),
        "restarted base never wrote its ready file\n{}",
        fixture.log()
    );
}

fn wait_for(path: &std::path::Path, exists: bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if path.exists() == exists {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.exists() == exists
}
