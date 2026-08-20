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

const NAME: &str = "ws-gate";
const TOKEN: &str = "ws-gate-token";
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

/// Spawns `tenon serve` and reads the bound URL off its stdout, so the test can
/// bind port 0 and never race a fixed port with another suite.
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

async fn connect(url: &str) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    let stream = tokio::net::TcpStream::connect(url).await.expect("tcp");
    let request = format!("ws://{url}/ws?token={TOKEN}");
    let (socket, _response) = tokio_tungstenite::client_async(request, stream)
        .await
        .expect("ws handshake");
    socket
}

async fn next_text(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("ws recv timed out")
            .expect("ws stream ended")
            .expect("ws message");
        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("json frame");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ws_client_subscribes_and_receives_a_coalesced_envelope() {
    let Some(fixture) = fixture(NAME) else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let (mut serve_child, url) = serve(&fixture, &["--auth-token", TOKEN]);
    let mut socket = connect(&url).await;

    socket
        .send(Message::Text(
            json!({"t": "bus.subscribe", "id": 1, "topics": ["session/**"], "coalesce_ms": 16})
                .to_string(),
        ))
        .await
        .expect("send subscribe");
    let reply = next_text(&mut socket).await;
    assert_eq!(reply["t"], "rep", "subscribe reply: {reply}");

    let mut publisher = Client::connect(&fixture.sock()).await.expect("uds");
    publisher
        .call(
            "bus.publish",
            json!({"envelope": {"topic": "session/ws.test", "env": "root",
                "durable": true, "payload": {"hello": "over-ws"}}}),
        )
        .await
        .expect("publish");

    let event = next_text(&mut socket).await;
    assert_eq!(event["t"], "ev", "expected an ev frame, got {event}");
    assert_eq!(event["topic"], "session/ws.test");
    assert_eq!(event["payload"]["hello"], "over-ws");

    let _ = serve_child.kill();
    let _ = serve_child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ws_upgrade_without_a_token_is_refused() {
    let Some(fixture) = fixture("ws-gate-auth") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let (mut serve_child, url) = serve(&fixture, &["--auth-token", TOKEN]);
    let stream = tokio::net::TcpStream::connect(&url).await.expect("tcp");
    let request = format!("ws://{url}/ws");
    let outcome = tokio_tungstenite::client_async(request, stream).await;
    assert!(outcome.is_err(), "unauthenticated ws upgrade should fail");

    let _ = serve_child.kill();
    let _ = serve_child.wait();
}
