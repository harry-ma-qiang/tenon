mod gate;

use gate::{kill_alive, skip_release, wait_gone, Fixture, Spec, BIN};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tenon_base::client::Client;
use tenon_test_support::{raw_connect, read_frame, send_raw};

/// No container: every test here only needs base's own facades. `sandbox:
/// none` also means the guardian and root are the only two nodes booted, and
/// crucially both are *real* registered envs with their own runtime token —
/// exactly what RFC 8d.2 env-scoping needs to test two genuinely different
/// envs without paying for `runtime.spawn` + oci.
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

fn token_of(fixture: &Fixture, env: &str) -> String {
    std::fs::read_to_string(fixture.home.join(format!("run/rt-{env}.token")))
        .unwrap_or_else(|error| panic!("no runtime token for {env}: {error}"))
        .trim()
        .to_string()
}

async fn scoped_client(fixture: &Fixture, env: &str) -> Client {
    let token = token_of(fixture, env);
    let mut c = client(fixture).await;
    c.call("auth.scope", json!({"env": env, "token": token}))
        .await
        .unwrap_or_else(|error| panic!("auth.scope({env}): {error}"));
    c
}

async fn next_ev(client: &mut Client, limit: Duration) -> Option<Value> {
    tokio::time::timeout(limit, client.next_ev())
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten()
}

/// Every `ev` frame a client receives inside `window`, whatever it is. Used
/// instead of a bare "did anything arrive" check where a connection may
/// legitimately see unrelated background traffic (base's own tracing
/// chatter tagged with the same env) under load: the test then inspects the
/// content instead of the mere fact that something arrived.
async fn collect_evs(client: &mut Client, window: Duration) -> Vec<Value> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match next_ev(client, remaining).await {
            Some(ev) => out.push(ev),
            None => break,
        }
    }
    out
}

fn restart(fixture: &Fixture) {
    let base = fixture.base_pid();
    let mut targets = fixture.node_pids();
    targets.push(base);
    kill_alive(base, "-9");
    wait_gone(&targets, Duration::from_secs(30));
    let (ok, text) = fixture.run_text(&["start"]);
    assert!(ok, "restart failed: {text}\n{}", fixture.log());
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

/// RFC 8d.2, the single most important P4 invariant, against two envs that
/// really exist (root and the guardian, both booted for free by `sandbox:
/// none`): a `**` subscribe and an exact-topic subscribe scoped to A must
/// receive nothing B publishes, A cannot range/watch/publish into B by
/// naming it, and an unscoped caller sees both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_real_envs_are_fully_isolated_and_cannot_write_into_each_other() {
    let Some(fixture) = fixture("adv-scope") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut a = scoped_client(&fixture, "root").await;
    let mut b = scoped_client(&fixture, "guardian").await;

    a.call("bus.subscribe", json!({"topics": ["**"]}))
        .await
        .expect("a firehose subscribe");
    let mut exact = scoped_client(&fixture, "root").await;
    exact
        .call("bus.subscribe", json!({"topics": ["leak/exact"]}))
        .await
        .expect("a exact-topic subscribe");

    b.call(
        "bus.publish",
        json!({"envelope": {"topic": "leak/exact", "durable": true, "payload": {"secret": 1}}}),
    )
    .await
    .expect("b publish");
    b.call(
        "bus.publish",
        json!({"envelope": {"topic": "leak/wild", "durable": true, "payload": {"secret": 2}}}),
    )
    .await
    .expect("b publish 2");

    // Any legitimate background chatter tagged env=root (base's own tracing,
    // node lifecycle events, ...) is allowed to reach A's firehose; only an
    // actual leak of B's envelope is a failure, so this checks content, not
    // mere arrival.
    let seen_by_a = collect_evs(&mut a, Duration::from_millis(700)).await;
    let leaked_to_a = seen_by_a
        .iter()
        .find(|ev| ev["env"] == "guardian" || ev["payload"]["secret"].is_number());
    assert!(
        leaked_to_a.is_none(),
        "env A's ** subscribe received env B's envelope: {leaked_to_a:?} (full batch: {seen_by_a:?})"
    );

    let seen_by_exact = collect_evs(&mut exact, Duration::from_millis(700)).await;
    let leaked_to_exact = seen_by_exact
        .iter()
        .find(|ev| ev["env"] == "guardian" || ev["payload"]["secret"].is_number());
    assert!(
        leaked_to_exact.is_none(),
        "env A's exact-topic subscribe received env B's envelope: {leaked_to_exact:?} (full batch: {seen_by_exact:?})"
    );

    // A cannot read, watch or write B by naming it explicitly.
    let range = a
        .call("kv.range", json!({"env": "guardian", "prefix": "/"}))
        .await;
    assert!(range.is_err(), "A read B's kv.range");
    assert!(range.unwrap_err().to_string().contains("cross_env_denied"));

    let watch = a
        .call("kv.watch", json!({"env": "guardian", "prefix": "/"}))
        .await;
    assert!(watch.is_err(), "A opened a kv.watch on B");

    let sub_b = a
        .call(
            "bus.subscribe",
            json!({"env": "guardian", "topics": ["**"]}),
        )
        .await;
    assert!(sub_b.is_err(), "A subscribed into B's env");

    let publish_into_b = a
        .call(
            "bus.publish",
            json!({"envelope": {"topic": "inject/x", "env": "guardian", "durable": true, "payload": {}}}),
        )
        .await;
    assert!(
        publish_into_b.is_err(),
        "A published into B's namespace by naming env=guardian"
    );

    // an unscoped caller (base/CLI) sees both envs' traffic.
    let mut root_cli = client(&fixture).await;
    root_cli
        .call(
            "bus.subscribe",
            json!({"topics": ["leak/**"], "since_offset": 0}),
        )
        .await
        .expect("unscoped subscribe");
    let first = next_ev(&mut root_cli, Duration::from_secs(3))
        .await
        .expect("unscoped caller missed the first leak envelope");
    let second = next_ev(&mut root_cli, Duration::from_secs(3))
        .await
        .expect("unscoped caller missed the second leak envelope");
    let topics: Vec<String> = [first, second]
        .iter()
        .map(|v| v["topic"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(topics.contains(&"leak/exact".to_string()));
    assert!(topics.contains(&"leak/wild".to_string()));
}

/// Blobs are documented (RFC section 8c/6) as capability-by-hash, not
/// per-env partitioned: possession of the sha256 is the read capability. This
/// nails that down as intended behaviour, not a scoping hole to "fix" by
/// surprise later.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_capability_is_shared_across_envs_by_hash() {
    let Some(fixture) = fixture("adv-blob-cap") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut b = scoped_client(&fixture, "guardian").await;
    let put = b
        .call("blob.put", json!({"data": b64("cross-env-bytes")}))
        .await
        .expect("b put");
    let hash = put["hash"].as_str().unwrap().to_string();

    let mut a = scoped_client(&fixture, "root").await;
    let got = a
        .call("blob.get", json!({"hash": hash}))
        .await
        .expect("A read B's blob by hash: blob scoping is capability-by-hash (documented)");
    assert_eq!(unb64(got["data"].as_str().unwrap()), b"cross-env-bytes");
}

/// A durable publish only resolves after its group-commit batch is
/// persisted, so an ack'd offset is a promise. This fires a burst of
/// concurrent durable publishes (their acks racing the 5 ms commit window),
/// kills base with -9 as soon as they are in flight, restarts, and checks
/// every acked event_id survived exactly once (event_id dedup — no
/// duplicates either).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn durable_kill_9_mid_batch_loses_nothing_acked_and_duplicates_nothing() {
    let Some(fixture) = fixture("adv-kill9") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let n = 400usize;
    let mut tasks = Vec::with_capacity(n);
    for i in 0..n {
        let sock = fixture.sock();
        tasks.push(tokio::spawn(async move {
            let mut c = match Client::connect(&sock).await {
                Ok(c) => c,
                Err(_) => return (i, false),
            };
            let event_id = format!("kill9-{i}");
            let outcome = c
                .call(
                    "bus.publish",
                    json!({"envelope": {
                        "topic": "kill9/x",
                        "durable": true,
                        "event_id": event_id,
                        "payload": {"i": i},
                    }}),
                )
                .await;
            (i, outcome.is_ok())
        }));
    }

    // Give the batch a moment to actually start landing, then kill hard
    // while more are still in flight.
    tokio::time::sleep(Duration::from_millis(15)).await;
    kill_alive(fixture.base_pid(), "-9");

    let mut acked: Vec<usize> = Vec::new();
    for task in tasks {
        if let Ok((i, ok)) = task.await {
            if ok {
                acked.push(i);
            }
        }
    }

    wait_gone(&[fixture.base_pid()], Duration::from_secs(30));
    let (ok, text) = fixture.run_text(&["start"]);
    assert!(ok, "restart failed: {text}\n{}", fixture.log());
    assert!(wait_for(
        &fixture.home.join("run/base.ready"),
        true,
        Duration::from_secs(30)
    ));
    wait_ready(&fixture).await;

    let mut reader = client(&fixture).await;
    reader
        .call(
            "bus.subscribe",
            json!({"topics": ["kill9/**"], "since_offset": 0}),
        )
        .await
        .expect("replay subscribe");
    let mut seen: Vec<i64> = Vec::new();
    while let Some(ev) = next_ev(&mut reader, Duration::from_millis(800)).await {
        seen.push(ev["payload"]["i"].as_i64().unwrap_or(-1));
    }

    let mut dedup = seen.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(
        dedup.len(),
        seen.len(),
        "the durable log has a duplicate event_id after kill -9 restart"
    );

    let missing: Vec<usize> = acked
        .iter()
        .copied()
        .filter(|i| !seen.contains(&(*i as i64)))
        .collect();
    assert!(
        missing.is_empty(),
        "acked durable envelopes were lost across kill -9: {missing:?} \
         ({} acked, {} replayed)",
        acked.len(),
        seen.len()
    );
    println!(
        "kill9 stress: {} of {n} publishes acked before the kill, {} replayed after restart",
        acked.len(),
        seen.len()
    );
}

/// Two concurrent `cas` on a fresh key with the same expectation: exactly one
/// wins. `incr` under concurrency converges to the exact count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kv_cas_race_has_one_winner_and_incr_converges_under_concurrency() {
    let Some(fixture) = fixture("adv-kv-race") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut setup = client(&fixture).await;
    setup
        .call(
            "kv.set",
            json!({"key": "/cas", "value": "0", "durable": true}),
        )
        .await
        .expect("seed cas key");

    let mut cas_tasks = Vec::new();
    for winner in 0..10 {
        let sock = fixture.sock();
        cas_tasks.push(tokio::spawn(async move {
            let mut c = Client::connect(&sock).await.expect("connect");
            c.call(
                "kv.cas",
                json!({"key": "/cas", "expect": "0", "value": format!("w{winner}"), "durable": true}),
            )
            .await
            .is_ok()
        }));
    }
    let mut wins = 0usize;
    for task in cas_tasks {
        if task.await.expect("cas task") {
            wins += 1;
        }
    }
    assert_eq!(wins, 1, "more than one cas won a race on the same key");

    setup
        .call(
            "kv.set",
            json!({"key": "/incr", "value": "0", "durable": true}),
        )
        .await
        .expect("seed incr key");
    let n = 50usize;
    let mut incr_tasks = Vec::new();
    for _ in 0..n {
        let sock = fixture.sock();
        incr_tasks.push(tokio::spawn(async move {
            let mut c = Client::connect(&sock).await.expect("connect");
            c.call(
                "kv.incr",
                json!({"key": "/incr", "delta": 1, "durable": true}),
            )
            .await
            .expect("incr")
        }));
    }
    for task in incr_tasks {
        task.await.expect("incr task");
    }
    let final_value = setup
        .call("kv.get", json!({"key": "/incr"}))
        .await
        .expect("final get");
    assert_eq!(
        final_value["value"],
        n.to_string(),
        "concurrent incr did not converge to the exact count"
    );
}

/// A lease's bound key is deleted on expiry, firing exactly one watch delete
/// event for it; `keep_alive` pinging faster than the ttl prevents expiry
/// entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kv_lease_expiry_fires_one_delete_and_keep_alive_prevents_it() {
    let Some(fixture) = fixture("adv-kv-lease") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut c = client(&fixture).await;
    let mut watch = client(&fixture).await;
    watch
        .call("kv.watch", json!({"prefix": "/lease/"}))
        .await
        .expect("watch");

    let dying = c
        .call("kv.lease", json!({"ttl_ms": 250}))
        .await
        .expect("lease")["lease_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.call(
        "kv.set",
        json!({"key": "/lease/dies", "value": "x", "durable": true, "lease_id": dying}),
    )
    .await
    .expect("bind dying key");

    let alive = c
        .call("kv.lease", json!({"ttl_ms": 400}))
        .await
        .expect("lease")["lease_id"]
        .as_str()
        .unwrap()
        .to_string();
    c.call(
        "kv.set",
        json!({"key": "/lease/lives", "value": "y", "durable": true, "lease_id": alive.clone()}),
    )
    .await
    .expect("bind alive key");

    let mut deletes_for_dying = 0usize;
    let mut saw_lives_delete = false;
    let keep_alive_until = Instant::now() + Duration::from_secs(2);
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if Instant::now() < keep_alive_until {
            let _ = c.call("kv.keep_alive", json!({"lease_id": alive})).await;
        }
        if let Some(ev) = next_ev(&mut watch, Duration::from_millis(150)).await {
            if ev["payload"]["op"] == "del" {
                if ev["payload"]["key"] == "/lease/dies" {
                    deletes_for_dying += 1;
                }
                if ev["payload"]["key"] == "/lease/lives" {
                    saw_lives_delete = true;
                }
            }
        }
        if Instant::now() >= keep_alive_until && deletes_for_dying >= 1 {
            break;
        }
    }

    assert_eq!(
        deletes_for_dying, 1,
        "the expired lease's key must fire exactly one watch delete"
    );
    assert!(
        !saw_lives_delete,
        "keep_alive did not prevent the pinged lease from expiring"
    );
    let still_there = c
        .call("kv.get", json!({"key": "/lease/lives"}))
        .await
        .expect("get lives");
    assert_eq!(
        still_there["found"], true,
        "the kept-alive lease's key is gone"
    );
}

/// RFC section 3: kv carries "a global monotonic revision". This checks that
/// promise survives a restart: a durable write, then a non-durable
/// (ephemeral) write that consumes a higher revision number, then restart,
/// then a fresh durable write must get a revision higher than anything
/// issued before the restart. `KvFacade::new` seeds its counter from
/// `kv_max_rev()` over the durable `kv` table only (rs/base/src/kv.rs), so a
/// revision consumed by an ephemeral write is forgotten on restart and can be
/// reissued to a different key — this is expected to fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kv_revision_stays_monotonic_across_a_restart() {
    let Some(fixture) = fixture("adv-kv-rev") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut c = client(&fixture).await;
    let durable_rev = c
        .call(
            "kv.set",
            json!({"key": "/rev/durable", "value": "1", "durable": true}),
        )
        .await
        .expect("durable set")["rev"]
        .as_i64()
        .unwrap();
    let ephemeral_rev = c
        .call(
            "kv.set",
            json!({"key": "/rev/ephemeral", "value": "1", "durable": false}),
        )
        .await
        .expect("ephemeral set")["rev"]
        .as_i64()
        .unwrap();
    assert!(ephemeral_rev > durable_rev, "setup: revision must advance");
    println!("DIAG durable_rev={durable_rev} ephemeral_rev={ephemeral_rev}");
    drop(c);

    restart(&fixture);

    let mut c2 = client(&fixture).await;
    let after_restart_durable_get = c2
        .call("kv.get", json!({"key": "/rev/durable"}))
        .await
        .expect("get durable after restart");
    println!("DIAG after_restart_durable_get={after_restart_durable_get}");
    let post_restart_rev = c2
        .call(
            "kv.set",
            json!({"key": "/rev/after", "value": "1", "durable": true}),
        )
        .await
        .expect("post-restart set")["rev"]
        .as_i64()
        .unwrap();
    println!("DIAG post_restart_rev={post_restart_rev}");

    assert!(
        post_restart_rev > ephemeral_rev,
        "revision went non-monotonic across restart: pre-restart ephemeral \
         write reached rev {ephemeral_rev}, but the first post-restart write \
         only reached rev {post_restart_rev} (KvFacade::new seeds only from \
         durable kv rows, see rs/base/src/kv.rs)"
    );
}

/// `after_ms` fires exactly once; `every_ms` keeps a cadence; `del` stops
/// further fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timer_after_ms_once_every_ms_cadence_and_del_stops_it() {
    let Some(fixture) = fixture("adv-timer") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut sub = client(&fixture).await;
    sub.call(
        "bus.subscribe",
        json!({"topics": ["adv/once", "adv/every"]}),
    )
    .await
    .expect("subscribe");
    let mut c = client(&fixture).await;
    c.call("timer.set", json!({"topic": "adv/once", "after_ms": 150}))
        .await
        .expect("after_ms set");

    let mut once_fires = 0usize;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match next_ev(&mut sub, Duration::from_millis(400)).await {
            Some(ev) if ev["topic"] == "adv/once" => once_fires += 1,
            _ => {}
        }
    }
    assert_eq!(once_fires, 1, "after_ms fired {once_fires} times, not once");

    let every_id = c
        .call(
            "timer.set",
            json!({"timer_id": "adv-every", "topic": "adv/every", "every_ms": 200}),
        )
        .await
        .expect("every_ms set")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let mut fire_times = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && fire_times.len() < 4 {
        if let Some(ev) = next_ev(&mut sub, Duration::from_millis(600)).await {
            if ev["topic"] == "adv/every" {
                fire_times.push(Instant::now());
            }
        }
    }
    assert!(
        fire_times.len() >= 3,
        "every_ms only fired {} times in 3s at a 200ms cadence",
        fire_times.len()
    );
    for pair in fire_times.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= Duration::from_millis(100),
            "every_ms fired twice too close together: {gap:?}"
        );
    }

    c.call("timer.del", json!({"timer_id": every_id}))
        .await
        .expect("del");
    let mut fires_after_del = 0usize;
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if let Some(ev) = next_ev(&mut sub, Duration::from_millis(300)).await {
            if ev["topic"] == "adv/every" {
                fires_after_del += 1;
            }
        }
    }
    assert_eq!(fires_after_del, 0, "timer.del did not stop the timer");
}

/// A missing topic, a `cron` field (unsupported in P4.0) and a negative
/// interval are all rejected with a clean error, not a crash; an oversized
/// `timer.set` payload is rejected the same way every oversized frame is
/// (the wire frame cap), and the base keeps serving other clients afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timer_set_rejects_bad_input_cleanly_and_the_server_survives() {
    let Some(fixture) = fixture("adv-timer-bad") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut c = client(&fixture).await;
    let no_topic = c.call("timer.set", json!({"after_ms": 100})).await;
    assert!(
        no_topic.is_err(),
        "timer.set with no topic must be rejected"
    );

    let cron = c
        .call("timer.set", json!({"topic": "adv/x", "cron": "* * * * *"}))
        .await;
    assert!(cron.is_err(), "cron must be rejected in P4.0");

    let negative = c
        .call("timer.set", json!({"topic": "adv/x", "after_ms": -5}))
        .await;
    assert!(negative.is_err(), "a negative interval must be rejected");

    // an oversized frame (bigger than base's 1 MiB frame cap) closes the
    // connection instead of crashing the server.
    let mut raw = raw_connect(&fixture.sock());
    let huge_payload = "x".repeat(2 * 1024 * 1024);
    let body = json!({
        "t": "timer.set",
        "id": 1,
        "topic": "adv/huge",
        "after_ms": 100,
        "payload": {"blob": huge_payload},
    });
    let _ = send_raw(&mut raw, serde_json::to_vec(&body).unwrap().as_slice());
    let reply = read_frame(&mut raw, Duration::from_secs(3));
    assert!(
        reply.is_err(),
        "an oversized timer.set frame should close the connection, not answer"
    );

    let mut still_alive = client(&fixture).await;
    let ok = still_alive
        .call("timer.set", json!({"topic": "adv/fine", "after_ms": 50}))
        .await;
    assert!(
        ok.is_ok(),
        "the server did not survive the oversized frame: {ok:?}"
    );
}

/// `blob.open` on an offset/len past the end of a stored blob and
/// `blob.get`/`blob.stat` of an unknown hash. The unknown-hash cases must
/// error cleanly (they do). The out-of-range window is documented in this
/// adversarial suite's brief as expected to "error cleanly"; the storage
/// layer instead clamps offset/len into range and returns an empty read, so
/// this assertion is expected to fail — see rs/storage/src/blobs.rs
/// `open_blob`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blob_open_out_of_range_errors_and_unknown_hash_errors() {
    let Some(fixture) = fixture("adv-blob-oob") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut c = client(&fixture).await;
    let put = c
        .call("blob.put", json!({"data": b64("hello")}))
        .await
        .expect("put");
    let hash = put["hash"].as_str().unwrap().to_string();

    let unknown_get = c.call("blob.get", json!({"hash": "not-a-real-hash"})).await;
    assert!(
        unknown_get.is_err(),
        "blob.get of an unknown hash must error"
    );
    let unknown_stat = c
        .call("blob.stat", json!({"hash": "not-a-real-hash"}))
        .await;
    assert!(
        unknown_stat.is_err(),
        "blob.stat of an unknown hash must error"
    );

    let out_of_range = c
        .call(
            "blob.open",
            json!({"hash": hash, "offset": 1_000_000, "len": 10}),
        )
        .await;
    assert!(
        out_of_range.is_err(),
        "blob.open with an offset past the blob's end should error cleanly, \
         but it returned {out_of_range:?} (open_blob clamps offset/len \
         instead of erroring, see rs/storage/src/blobs.rs)"
    );
}

/// `bus.subscribe` with a filter shaped wrong (topics not an array, levels
/// full of garbage strings, a session of the wrong type) must not crash the
/// server: the front door either answers with a clean error or degrades to
/// "no constraint on that axis", but the connection stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_subscribe_filters_do_not_crash_the_server() {
    let Some(fixture) = fixture("adv-malformed") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut c = client(&fixture).await;
    let _ = c.call("bus.subscribe", json!({"topics": 12345})).await;
    let _ = c
        .call("bus.subscribe", json!({"levels": ["not-a-level", 7, null]}))
        .await;
    let _ = c
        .call("bus.subscribe", json!({"session": {"nested": true}}))
        .await;
    let _ = c
        .call("bus.subscribe", json!({"coalesce_ms": "not-a-number"}))
        .await;
    let _ = c.call("bus.subscribe", json!("not-even-an-object")).await;

    let mut still_alive = client(&fixture).await;
    let ok = still_alive
        .call("bus.subscribe", json!({"topics": ["ok/**"]}))
        .await;
    assert!(
        ok.is_ok(),
        "the server did not survive a batch of malformed subscribe filters: {ok:?}"
    );
}

/// RFC section 2 lists `internal/ base/ budget/ approval/ guardian/ upgrade/
/// worker/` as reserved namespaces, but no code path in bus_publish checks a
/// topic against that list (rs/base/src/facaderpc.rs `bus_publish`). This
/// documents the current, permissive behaviour rather than assuming it: a
/// connection scoped to a normal env can publish onto a reserved-looking
/// topic under its own env with nothing refusing it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scoped_connection_can_currently_publish_into_reserved_namespaces() {
    let Some(fixture) = fixture("adv-reserved") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut a = scoped_client(&fixture, "root").await;
    for topic in [
        "internal/spoof",
        "guardian/reset",
        "base/boot",
        "upgrade/phase",
    ] {
        let result = a
            .call(
                "bus.publish",
                json!({"envelope": {"topic": topic, "durable": true, "payload": {}}}),
            )
            .await;
        assert!(
            result.is_ok(),
            "expected the current (unenforced) behaviour: a plugin env \
             publishing onto reserved topic {topic:?} is accepted, got {result:?}. \
             If this now fails, RFC section 2's reserved namespaces have \
             gained enforcement and this test's assumption is stale."
        );
    }
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
