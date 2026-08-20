#![cfg(feature = "http")]

mod gate;

use futures_util::{SinkExt, StreamExt};
use gate::{skip_release, Fixture, Spec, BIN};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::Child;
use std::time::Duration;
use tenon_base::client::Client;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

const TOKEN: &str = "ws-scope-adv-token";
/// `sandbox: none` boots exactly two real, independently-tokened envs (root
/// and guardian) for free, which is what every RFC 8d.2 env-scope test in
/// this repo builds on (see `bus_adversarial.rs`).
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

fn serve(fixture: &Fixture, extra: &[&str]) -> (Child, String) {
    let mut args = vec!["serve", "--http", "127.0.0.1:0"];
    args.extend_from_slice(extra);
    let mut child = fixture.spawn(&args);
    let stdout = child.stdout.take().expect("serve stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if reader.read_line(&mut line).expect("read serve stdout") == 0 {
            break;
        }
        if let Some(index) = line.find("://") {
            let url = line[index + 3..].trim().to_string();
            return (child, url);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("serve never printed its address");
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

type Ws = WebSocketStream<tokio::net::TcpStream>;

async fn connect(url: &str, token: &str) -> Ws {
    let stream = tokio::net::TcpStream::connect(url).await.expect("tcp");
    let request = format!("ws://{url}/ws?token={token}");
    let (socket, _response) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("ws handshake");
    socket
}

/// Sends one `{"t": method, "id": N, ...params}` frame and waits for the
/// matching `t:"rep"` reply, skipping any `t:"ev"` push frames a live
/// subscription might interleave -- exactly the shape `tenon_base::Client`
/// uses over the UDS carrier, so this is what a WS-carrier caller looks like
/// from the browser side.
async fn ws_call(socket: &mut Ws, id: u64, method: &str, params: Value) -> Value {
    let mut frame = json!({"t": method, "id": id});
    if let (Some(target), Some(extra)) = (frame.as_object_mut(), params.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    socket
        .send(Message::Text(frame.to_string()))
        .await
        .expect("send ws call");
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("ws recv timed out")
            .expect("ws stream ended")
            .expect("ws message");
        if let Message::Text(text) = message {
            let value: Value = serde_json::from_str(&text).expect("json frame");
            if value["t"] == "rep" && value["id"].as_u64() == Some(id) {
                return value;
            }
        }
    }
}

fn token_of(fixture: &Fixture, env: &str) -> String {
    std::fs::read_to_string(fixture.home.join(format!("run/rt-{env}.token")))
        .unwrap_or_else(|error| panic!("no runtime token for {env}: {error}"))
        .trim()
        .to_string()
}

/// RFC 8d.2 calls env-scoping "the single most important P4 invariant":
/// a caller bounded to one env must never read or write another's kv/bus/
/// blob/query. A raw UDS caller that calls `auth.scope` first is correctly
/// denied cross-env (`bus_adversarial.rs` proves this). This is the same
/// check against the WS carrier: the WS bridge in `rs/base/src/ws.rs` opens
/// a brand-new UDS connection to base's own front door and never calls
/// `auth.scope` on it, so every WS client -- authenticated with nothing more
/// than the single shared bearer token -- rides in as an *unscoped* base/CLI
/// caller for the whole host, able to name any env explicitly. `serve --env
/// root` gives no indication this is happening: the flag only picks which
/// env's HTML page renders.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_bearer_token_client_can_read_and_write_an_env_other_than_the_one_serve_named() {
    let Some(fixture) = fixture("ws-scope-kv") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--env", "root", "--auth-token", TOKEN]);
    let mut socket = connect(&url, TOKEN).await;

    let set = ws_call(
        &mut socket,
        1,
        "kv.set",
        json!({"env": "guardian", "key": "/leak", "value": "written-over-ws"}),
    )
    .await;
    assert!(
        set.get("error").is_none(),
        "sanity: kv.set into guardian answered {set}"
    );

    let get = ws_call(
        &mut socket,
        2,
        "kv.get",
        json!({"env": "guardian", "key": "/leak"}),
    )
    .await;
    assert_eq!(
        get.get("error").and_then(Value::as_str),
        Some("cross_env_denied"),
        "a WS client authenticated only with serve's bearer token wrote into and \
         read back env `guardian` while serve was started with --env root, with no \
         auth.scope ever binding the connection to any env: {get} (write reply: {set})"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// The RFC 8d.4 secrets facade promises per-env `grants`: "an unscoped
/// base/CLI caller always reads; a scoped env reads only what `grants`
/// lists." A WS client is never scoped (see the test above), so
/// `secret.get` treats it exactly like the local `tenon` CLI -- meaning the
/// grants list is meaningless for any secret reachable through a running
/// `serve`. This sets `grants: []` (nobody, not even root) and shows a bare
/// WS client with only the bearer token reads it anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_bearer_token_client_reads_a_secret_granted_to_no_env_at_all() {
    let Some(fixture) = fixture("ws-scope-secret") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut admin = Client::connect(&fixture.sock()).await.expect("uds");
    admin
        .call(
            "secret.set",
            json!({"name": "nobody-secret", "value": "sk-not-for-any-env", "leak": "mask", "grants": []}),
        )
        .await
        .expect("set secret with no grants");

    let (mut child, url) = serve(&fixture, &["--env", "root", "--auth-token", TOKEN]);
    let mut socket = connect(&url, TOKEN).await;

    let get = ws_call(
        &mut socket,
        1,
        "secret.get",
        json!({"name": "nobody-secret"}),
    )
    .await;
    assert_eq!(
        get.get("error").and_then(Value::as_str),
        Some("not_granted"),
        "a WS client holding only the bearer token read a secret granted to no env: {get}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// The same unscoped-connection root cause reaches well past the four named
/// facades: `config.get` (and everything else `server.rs::dispatch` routes
/// off a raw `env` field from the request body) never consults
/// `conn.scoped_env` at all, for *any* caller, scoped or not. This shows a
/// WS client can read another env's harness/runtime config even though it
/// only ever proved knowledge of the one shared serve token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ws_bearer_token_client_reads_another_envs_config() {
    let Some(fixture) = fixture("ws-scope-config") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--env", "root", "--auth-token", TOKEN]);
    let mut socket = connect(&url, TOKEN).await;

    let config = ws_call(&mut socket, 1, "config.get", json!({"env": "guardian"})).await;
    assert!(
        config.get("error").is_some(),
        "a WS client scoped to nothing but the bearer token read env `guardian`'s \
         config through a serve process started with --env root: {config}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// A raw UDS caller that *does* bind itself with `auth.scope{env, token}` is
/// the RFC's own trusted-scoped-plugin story. This proves the same
/// dispatch-level gap from the other side: even a genuinely scoped
/// connection (root's own runtime token) is not stopped from naming a
/// different env on `config.get`, because that method never calls
/// `conn.scoped_env`. This is a defect for *any* carrier, not just WS.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scoped_uds_connection_still_reads_another_envs_config_by_naming_it() {
    let Some(fixture) = fixture("ws-scope-config-uds") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let token = token_of(&fixture, "root");
    let mut scoped = Client::connect(&fixture.sock()).await.expect("uds");
    scoped
        .call("auth.scope", json!({"env": "root", "token": token}))
        .await
        .expect("scope to root");

    let config = scoped.call("config.get", json!({"env": "guardian"})).await;
    assert!(
        config.is_err(),
        "a connection scoped to `root` via auth.scope read env `guardian`'s config \
         by simply naming it: {config:?}"
    );

    let _ = fixture;
}

/// WS carrier robustness (RFC P4.4 acceptance: "feature off = binary
/// unchanged", and implicitly, feature on must not let one bad client take
/// the server down). A non-JSON text frame and a binary frame (reserved for
/// media, "accepted and ignored for now" per the README) must both be
/// silently absorbed, and the same connection must still answer a
/// well-formed call afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_text_and_binary_frames_do_not_kill_the_connection_or_serve() {
    let Some(fixture) = fixture("ws-scope-malformed") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--env", "root", "--auth-token", TOKEN]);
    let mut socket = connect(&url, TOKEN).await;

    socket
        .send(Message::Text("not json at all {{{".to_string()))
        .await
        .expect("send malformed text");
    socket
        .send(Message::Binary(vec![0u8, 1, 2, 3, 255, 254]))
        .await
        .expect("send binary frame");

    let status = ws_call(&mut socket, 1, "status", json!({})).await;
    assert!(
        status.get("error").is_none(),
        "the connection should still answer a well-formed call after garbage frames: {status}"
    );

    let mut probe = Client::connect(&fixture.sock())
        .await
        .expect("uds still up");
    let ok = probe.call("status", json!({})).await;
    assert!(ok.is_ok(), "base itself should still be healthy: {ok:?}");

    let _ = child.kill();
    let _ = child.wait();
}

/// A frame that decodes to valid JSON but is too large for the UDS wire
/// (`frame::MAX_FRAME`, 1 MiB) breaks that one client's uplink pump (the
/// bridge cannot forward it), but must not take the rest of `serve` or base
/// down with it: a fresh connection right after must work normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversized_frame_only_costs_its_own_connection() {
    let Some(fixture) = fixture("ws-scope-oversized") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--env", "root", "--auth-token", TOKEN]);
    let mut socket = connect(&url, TOKEN).await;

    let huge = "x".repeat(2 * 1024 * 1024);
    let frame = json!({"t": "kv.set", "id": 1, "key": "/big", "value": huge});
    let _ = socket.send(Message::Text(frame.to_string())).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), socket.next()).await;

    let mut fresh = connect(&url, TOKEN).await;
    let status = ws_call(&mut fresh, 1, "status", json!({})).await;
    assert!(
        status.get("error").is_none(),
        "a fresh WS connection after an oversized frame should still work: {status}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// An abrupt disconnect (TCP drop, no WS close frame) must still release the
/// server-side subscription/connection state -- the same guarantee a clean
/// close gets. `status.attached` (RFC section 8: UI-on-subscribe) is the
/// existing counter for this: it must return to its pre-connection value
/// once the drop is noticed, not leak forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_abrupt_close_without_a_ws_close_frame_still_cleans_up_the_subscription() {
    let Some(fixture) = fixture("ws-scope-abrupt") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--env", "root", "--auth-token", TOKEN]);

    let mut baseline_probe = Client::connect(&fixture.sock()).await.expect("uds");
    let baseline = baseline_probe
        .call("status", json!({}))
        .await
        .expect("status")["attached"]
        .as_u64()
        .unwrap_or(0);

    let mut socket = connect(&url, TOKEN).await;
    let sub = ws_call(&mut socket, 1, "bus.subscribe", json!({"topics": ["**"]})).await;
    assert!(sub.get("error").is_none(), "subscribe should work: {sub}");

    let mut during_probe = Client::connect(&fixture.sock()).await.expect("uds");
    let during = during_probe
        .call("status", json!({}))
        .await
        .expect("status")["attached"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        during > baseline,
        "attached should rise after a live WS subscribe: baseline {baseline}, during {during}"
    );

    drop(socket);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut settled = during;
    while tokio::time::Instant::now() < deadline {
        let mut probe = Client::connect(&fixture.sock()).await.expect("uds");
        settled = probe.call("status", json!({})).await.expect("status")["attached"]
            .as_u64()
            .unwrap_or(0);
        if settled <= baseline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        settled <= baseline,
        "attached never dropped back after an abrupt WS disconnect: baseline {baseline}, \
         settled at {settled}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// A flood of rapid frames (some tiny bus.subscribe/kv churn) must not wedge
/// or crash serve; the connection should keep answering throughout and base
/// should be healthy afterward.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_frames_does_not_crash_or_wedge_serve() {
    let Some(fixture) = fixture("ws-scope-flood") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--env", "root", "--auth-token", TOKEN]);
    let mut socket = connect(&url, TOKEN).await;

    for id in 1..=400u64 {
        let frame = json!({"t": "kv.set", "id": id, "key": format!("/flood/{id}"), "value": "x"});
        socket
            .send(Message::Text(frame.to_string()))
            .await
            .expect("send flood frame");
    }

    let mut answered = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while answered < 400 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if value["t"] == "rep" {
                    answered += 1;
                }
            }
            _ => break,
        }
    }
    assert_eq!(answered, 400, "every flood frame should still get a reply");

    let mut probe = Client::connect(&fixture.sock())
        .await
        .expect("uds still up");
    let ok = probe.call("status", json!({})).await;
    assert!(ok.is_ok(), "base should be healthy after the flood: {ok:?}");

    let _ = child.kill();
    let _ = child.wait();
}
