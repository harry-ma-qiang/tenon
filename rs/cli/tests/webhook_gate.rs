#![cfg(feature = "http")]

mod gate;

use gate::{skip_release, Fixture, Spec, BIN};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const NAME: &str = "webhook-gate";
const TOKEN: &str = "webhook-gate-token";

/// A raw HTTP request with an optional bearer header; returns the status code
/// and the whole response text.
async fn http(address: &str, request: &str, token: Option<&str>, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).await.expect("connect");
    let auth = match token {
        Some(token) => format!("Authorization: Bearer {token}\r\n"),
        None => String::new(),
    };
    let head = format!(
        "{request}\r\nHost: {address}\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.expect("send");
    stream.flush().await.expect("flush");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    let text = String::from_utf8_lossy(&raw).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    (status, text)
}

async fn read_address(child: &mut std::process::Child) -> String {
    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read address");
    line.split("http://")
        .nth(1)
        .map(|rest| {
            rest.split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches(',')
                .to_string()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbound_webhook_publishes_authorizes_and_caps_the_body() {
    let Some(release) = skip_release(NAME) else {
        return;
    };
    let fixture = Fixture::open(
        BIN,
        release,
        Spec {
            name: NAME,
            config: Some("sandbox: none\ntriggers:\n  webhook_body_limit: 1024\n"),
            ..Spec::default()
        },
    );
    fixture.start();
    fixture.registered("root", Duration::from_secs(60)).await;

    let mut child = fixture.spawn(&["serve", "--http", "127.0.0.1:0", "--auth-token", TOKEN]);
    let address = read_address(&mut child).await;
    assert!(!address.is_empty(), "serve did not print its address");

    // A subscriber on the base front door sees what the hook publishes.
    let mut sub = fixture.client().await;
    sub.call("bus.subscribe", json!({"topics": ["hook/**"]}))
        .await
        .expect("subscribe");

    // token -> publish.
    let (status, body) = http(
        &address,
        "POST /hook/ci HTTP/1.1",
        Some(TOKEN),
        "{\"ref\":\"main\"}",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), sub.next_ev()).await {
            Ok(Ok(Some(event))) if event["topic"] == json!("hook/ci") => {
                assert_eq!(event["payload"]["ref"], json!("main"), "{event}");
                assert_eq!(event["env"], json!("root"), "{event}");
                seen = true;
                break;
            }
            Ok(Ok(Some(_))) => continue,
            _ => break,
        }
    }
    assert!(seen, "the hook did not publish an envelope");

    // no token -> 401.
    let (status, _body) = http(&address, "POST /hook/ci HTTP/1.1", None, "{}").await;
    assert_eq!(status, 401);

    // oversized body -> 413.
    let big = "x".repeat(2048);
    let (status, _body) = http(&address, "POST /hook/ci HTTP/1.1", Some(TOKEN), &big).await;
    assert_eq!(status, 413);

    let _ = child.kill();
    let _ = child.wait();
    fixture.run(&["stop"]);
}
