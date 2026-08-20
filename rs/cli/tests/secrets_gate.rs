#![cfg(feature = "http")]

mod gate;

use gate::{skip_release, Fixture, Spec, BIN};
use serde_json::{json, Value};
use std::time::Duration;
use tenon_base::client::Client;

const NAME: &str = "secrets-gate";
const CONFIG: &str = "sandbox: none\n";
const HARNESS: &str = "llm:\n  provider: openai\n  base_url: http://127.0.0.1:1\n  \
     model: fake-model\n  api_key_env: TENON_TEST_NO_KEY\nmax_steps: 2\napproval: deny\n";
const VALUE: &str = "sk-SECRET-abc-123";

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

async fn wait_ready(fixture: &Fixture) {
    let ok = fixture
        .await_status(Duration::from_secs(120), |status| {
            status["nodes"]
                .as_array()
                .map(|nodes| nodes.len() >= 2)
                .unwrap_or(false)
        })
        .await;
    assert!(ok, "base never came up\n{}", fixture.log());
}

async fn subscribe(fixture: &Fixture, topics: Value) -> Client {
    let mut client = Client::connect(&fixture.sock()).await.expect("uds");
    client
        .call(
            "bus.subscribe",
            json!({"topics": topics, "since_offset": 0}),
        )
        .await
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mask_rewrites_a_leaked_value_and_block_refuses_with_a_violation() {
    let Some(fixture) = fixture(NAME) else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut base = Client::connect(&fixture.sock()).await.expect("uds");
    base.call(
        "secret.set",
        json!({"name": "api", "value": VALUE, "leak": "mask", "grants": ["root"]}),
    )
    .await
    .expect("set mask secret");
    base.call(
        "secret.set",
        json!({"name": "killswitch", "value": "BLOCK-ME-NOW", "leak": "block"}),
    )
    .await
    .expect("set block secret");

    // mask: a subscriber sees ***api***, never the value.
    let mut watcher = subscribe(&fixture, json!(["session/**"])).await;
    base.call(
        "bus.publish",
        json!({"envelope": {"topic": "session/leak", "env": "root", "durable": true,
            "payload": {"tail": format!("used {VALUE} in a command")}}}),
    )
    .await
    .expect("publish mask");
    let masked = loop {
        let event = next_ev(&mut watcher, Duration::from_secs(10)).await;
        if event["topic"] == "session/leak" {
            break event;
        }
    };
    let tail = masked["payload"]["tail"].as_str().unwrap_or_default();
    assert_eq!(tail, "used ***api*** in a command", "masked tail: {masked}");
    assert!(!tail.contains(VALUE), "value leaked: {masked}");

    // block: the publish is refused and a violation is emitted.
    let mut violations = subscribe(&fixture, json!(["guardian/**"])).await;
    let refused = base
        .call(
            "bus.publish",
            json!({"envelope": {"topic": "session/danger", "env": "root", "durable": true,
                "payload": {"note": "here is BLOCK-ME-NOW plainly"}}}),
        )
        .await;
    assert!(refused.is_err(), "block publish should be refused");
    let violation = loop {
        let event = next_ev(&mut violations, Duration::from_secs(10)).await;
        if event["topic"] == "guardian/violation" {
            break event;
        }
    };
    assert_eq!(violation["payload"]["secret"], "killswitch");

    // The event log never holds the raw value: replay every durable envelope.
    let mut replay = subscribe(&fixture, json!(["session/**"])).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), replay.next_ev()).await {
            Ok(Ok(Some(event))) => {
                assert!(!event.to_string().contains(VALUE), "log leaked: {event}");
                assert!(
                    !event.to_string().contains("BLOCK-ME-NOW"),
                    "blocked value persisted: {event}"
                );
            }
            _ => break,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scoped_env_reads_only_a_secret_it_is_granted() {
    let Some(fixture) = fixture("secrets-grant") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut base = Client::connect(&fixture.sock()).await.expect("uds");
    base.call(
        "secret.set",
        json!({"name": "shared", "value": "grant-ok", "leak": "mask", "grants": ["root"]}),
    )
    .await
    .expect("set granted");
    base.call(
        "secret.set",
        json!({"name": "private", "value": "no-touch", "leak": "mask", "grants": ["other"]}),
    )
    .await
    .expect("set ungranted");

    // The unscoped base caller reads any secret.
    let granted = base
        .call("secret.get", json!({"name": "shared"}))
        .await
        .expect("base get");
    assert_eq!(granted["value"], "grant-ok");

    // A connection scoped to root reads what root is granted, and nothing else.
    let token = std::fs::read_to_string(fixture.home.join("run/rt-root.token"))
        .expect("runtime token")
        .trim()
        .to_string();
    let mut scoped = Client::connect(&fixture.sock()).await.expect("uds");
    scoped
        .call("auth.scope", json!({"env": "root", "token": token}))
        .await
        .expect("scope");
    let ok = scoped
        .call("secret.get", json!({"name": "shared"}))
        .await
        .expect("scoped get granted");
    assert_eq!(ok["value"], "grant-ok");
    let denied = scoped.call("secret.get", json!({"name": "private"})).await;
    assert_eq!(
        denied.err().map(|error| error.to_string()).as_deref(),
        Some("not_granted")
    );

    // secret.list never carries a value.
    let listed = base.call("secret.list", json!({})).await.expect("list");
    assert!(
        !listed.to_string().contains("no-touch") && !listed.to_string().contains("grant-ok"),
        "list leaked a value: {listed}"
    );
}
