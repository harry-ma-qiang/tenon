mod gate;

use gate::{skip, Fixture, BIN};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tenon_harness::fake::{self, Say};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const NAME: &str = "ui-gate";

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 2\napproval: ask\n"
    )
}

/// `tenon attach --ui` under a real pty (`script -q`), driven by one keystroke:
/// the frame carries the tree and the borders the renderer draws, and `q`
/// leaves the terminal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attach_ui_renders_a_frame_in_a_pty_and_quits_on_q() {
    let Some(release) = skip(NAME) else { return };
    let server = fake::spawn(vec![]).await.expect("fake model");
    let fixture = Fixture::new(NAME, release, "sandbox: oci\n", &harness(&server.base_url));
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    let home = fixture.home.display().to_string();
    let line = format!("{BIN} --home {home} attach --ui");
    let mut child = Command::new("script")
        .args(["-q", "-c", &line, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn script");
    tokio::time::sleep(Duration::from_secs(3)).await;
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin.write_all(b"q").expect("send q");
        stdin.flush().expect("flush");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait().expect("wait") {
            Some(_status) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("attach --ui did not quit on q\n{}", fixture.log());
            }
            None => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    let output = child.wait_with_output().expect("collect");
    let frame = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        frame.contains("\x1b[2J"),
        "no clear in the frame: {frame:?}"
    );
    assert!(frame.contains("root"), "no env in the frame: {frame:?}");
    assert!(
        frame.contains("+-"),
        "no ascii border in the frame: {frame:?}"
    );
    assert!(frame.contains("q quit"), "no input hint: {frame:?}");
}

/// `tenon serve --http`: the same renderer, one render per request, forms
/// instead of keys. GET is a page, POST /prompt drives a turn and
/// POST /approve/<id> answers the queue.
#[cfg(feature = "http")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_http_renders_the_page_prompts_and_answers_an_approval() {
    let Some(release) = skip(NAME) else { return };
    let server = fake::spawn(vec![Say::Text("web pong".to_string())])
        .await
        .expect("fake model");
    let fixture = Fixture::new(
        "http-gate",
        release,
        "sandbox: oci\n",
        &harness(&server.base_url),
    );
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    let mut child = fixture.spawn(&["serve", "--http", "127.0.0.1:0"]);
    let address = read_address(&mut child).await;

    // a. the page renders the tree
    let (status, body) = http(&address, "GET / HTTP/1.1", "").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("<pre>"), "{body}");
    assert!(body.contains("root"), "{body}");
    assert!(body.contains("action=\"/prompt\""), "{body}");

    // b. a form post drives a real turn
    let (status, _body) = http(&address, "POST /prompt HTTP/1.1", "text=say+web+pong").await;
    assert_eq!(status, 303);
    let answered = await_kind(&fixture, "turn/end", Duration::from_secs(60)).await;
    assert_eq!(answered["ok"], true, "{answered}");

    // c. a pending approval is answered through the page
    let sock = fixture.home.join("run/base.sock");
    let asking = tokio::spawn(async move {
        let mut client = tenon_base::client::Client::connect(&sock)
            .await
            .expect("connect");
        client
            .call(
                "approval.request",
                json!({"env": "root", "reason": "push the workspace out", "kind": "test"}),
            )
            .await
    });
    let id = fixture
        .await_approval("push the workspace out", Duration::from_secs(30))
        .await;
    let (status, _body) = http(
        &address,
        &format!("POST /approve/{id} HTTP/1.1"),
        "decision=approve",
    )
    .await;
    assert_eq!(status, 303);
    let verdict = asking.await.expect("join").expect("approval.request");
    assert_eq!(verdict["status"], "approved", "{verdict}");

    let (status, body) = http(&address, "GET /nope HTTP/1.1", "").await;
    assert_eq!(status, 404, "{body}");
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(feature = "http")]
async fn read_address(child: &mut std::process::Child) -> String {
    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read the address");
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

#[cfg(feature = "http")]
async fn http(address: &str, request: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).await.expect("connect http");
    let head = format!(
        "{request}\r\nHost: {address}\r\nContent-Type: application/x-www-form-urlencoded\r\n\
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

#[cfg(feature = "http")]
async fn await_kind(fixture: &Fixture, kind: &str, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(row) = fixture.of_kind(kind).await.last() {
            return row.clone();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    panic!("no {kind} within {limit:?}\n{}", fixture.log());
}
