#![cfg(feature = "http")]

mod gate;

use gate::{skip, Fixture, Spec, BIN};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use tenon_harness::fake::{self, Fake, Say};

const NAME: &str = "serve-https-gate";
const TOKEN: &str = "https-gate-token";

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 2\napproval: deny\n"
    )
}

/// Spawns `tenon serve --https`, returning the bound URL and the self-signed
/// fingerprint line it printed before it.
fn serve(fixture: &Fixture) -> (Child, String, String) {
    let mut child = fixture.spawn(&[
        "serve",
        "--https",
        "--http",
        "127.0.0.1:0",
        "--auth-token",
        TOKEN,
    ]);
    let stdout = child.stdout.take().expect("serve stdout");
    let mut reader = BufReader::new(stdout);
    let mut fingerprint = String::new();
    let mut line = String::new();
    for _ in 0..20 {
        line.clear();
        if reader.read_line(&mut line).expect("read serve stdout") == 0 {
            break;
        }
        if line.contains("sha-256") {
            fingerprint = line.trim().to_string();
        }
        if let Some(index) = line.find("://") {
            let url = line[index + 3..].trim().to_string();
            return (child, url, fingerprint);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("serve never printed its address");
}

/// `curl -sk` (accepts the self-signed cert), returning the status code and the
/// body. The token is passed as a bearer header when one is given.
fn curl(url: &str, method: &str, token: Option<&str>, form: Option<&str>) -> (u16, String) {
    let mut command = Command::new("curl");
    command.arg("-sk").arg("-X").arg(method);
    command.arg("-w").arg("\n%{http_code}");
    if let Some(token) = token {
        command
            .arg("-H")
            .arg(format!("Authorization: Bearer {token}"));
    }
    if let Some(form) = form {
        command.arg("--data").arg(form);
    }
    let output = command.arg(url).output().expect("run curl");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or((&text, "0"));
    (code.trim().parse().unwrap_or(0), body.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_serves_the_ui_behind_the_bearer_token_and_drives_a_turn() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let server: Fake = fake::spawn(vec![Say::Text("served-pong".to_string())])
        .await
        .expect("fake model");
    let fixture = Fixture::open(
        BIN,
        release,
        Spec {
            name: NAME,
            config: Some("sandbox: oci\n"),
            harness: Some(&harness(&server.base_url)),
            reap_pids: true,
            lock: true,
            limit: Some(Duration::from_secs(180)),
        },
    );
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    let (mut serve_child, url, fingerprint) = serve(&fixture);
    let base = format!("https://{url}");
    assert!(
        fingerprint.contains("sha-256"),
        "no cert fingerprint printed: {fingerprint:?}"
    );

    // No token is 401; a wrong token is 401; the right token renders the UI.
    let (code, _body) = curl(&base, "GET", None, None);
    assert_eq!(code, 401, "unauthenticated GET should be 401");
    let (code, _body) = curl(&base, "GET", Some("wrong"), None);
    assert_eq!(code, 401, "a wrong token should be 401");
    let (code, body) = curl(&base, "GET", Some(TOKEN), None);
    assert_eq!(code, 200, "authenticated GET should be 200: {body}");
    assert!(body.contains("<pre"), "no <pre> UI in body: {body}");
    assert!(body.contains("root"), "env name missing from UI: {body}");

    // POST /prompt drives one fake-model turn.
    let prompt = format!("{base}/prompt");
    let (code, _body) = curl(&prompt, "POST", Some(TOKEN), Some("text=hello+there"));
    assert!(code == 303 || code == 200, "prompt should redirect: {code}");

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && server.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(
        !server.requests().is_empty(),
        "the fake model was never called through POST /prompt\n{}",
        fixture.log()
    );

    let _ = serve_child.kill();
    let _ = serve_child.wait();
}
