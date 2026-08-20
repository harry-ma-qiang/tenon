#![cfg(feature = "http")]

mod gate;

use gate::{skip_release, Fixture, Spec, BIN};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::Child;
use std::time::Duration;

const NAME: &str = "serve-authz-adv";
const TOKEN: &str = "adv-token-0123456789";
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

/// Sends a raw HTTP/1.1 request byte-for-byte as written (so the test can
/// control header casing, duplication and whitespace precisely, which a
/// higher-level client such as `curl` or `reqwest` would normalise away) and
/// reads until the server closes the connection (every reply here carries
/// `Connection: close`). Returns the status line's code and the whole
/// response text.
fn raw_request(addr: &str, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).expect("tcp connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    (status, text)
}

fn get(addr: &str, path: &str, auth_header: Option<&str>) -> (u16, String) {
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n");
    if let Some(header) = auth_header {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("Connection: close\r\n\r\n");
    raw_request(addr, &request)
}

fn post_prompt(addr: &str, auth_header: Option<&str>, body: &str) -> (u16, String) {
    let mut request = format!("POST /prompt HTTP/1.1\r\nHost: {addr}\r\n");
    if let Some(header) = auth_header {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
    request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    request.push_str("Connection: close\r\n\r\n");
    request.push_str(body);
    raw_request(addr, &request)
}

fn ws_upgrade(addr: &str, auth_header: Option<&str>) -> (u16, String) {
    let mut request = format!("GET /ws HTTP/1.1\r\nHost: {addr}\r\n");
    if let Some(header) = auth_header {
        request.push_str(header);
        request.push_str("\r\n");
    }
    request.push_str("Upgrade: websocket\r\nConnection: Upgrade\r\n");
    request.push_str("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n");
    request.push_str("Sec-WebSocket-Version: 13\r\n\r\n");
    raw_request(addr, &request)
}

/// RFC 8d.1: every serve carrier passes through the one authorizer. This
/// walks GET /, POST /prompt and GET /ws (the upgrade path) against a
/// missing, empty, wrong and truncated-real token, plus an unknown path,
/// and expects 401 from every one of them -- auth runs before routing, so
/// even a 404 candidate must not leak past the gate unauthenticated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_route_rejects_missing_empty_wrong_and_truncated_tokens() {
    let Some(fixture) = fixture(NAME) else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--auth-token", TOKEN]);

    let truncated = &TOKEN[..TOKEN.len() - 1];
    let bad_tokens: Vec<Option<String>> = vec![
        None,
        Some(String::new()),
        Some("totally-wrong-token".to_string()),
        Some(truncated.to_string()),
    ];

    for bad in &bad_tokens {
        let header = bad
            .as_ref()
            .map(|token| format!("Authorization: Bearer {token}"));
        let (code, body) = get(&url, "/", header.as_deref());
        assert_eq!(code, 401, "GET / with token {bad:?} should be 401: {body}");

        let (code, body) = post_prompt(&url, header.as_deref(), "text=hi");
        assert_eq!(
            code, 401,
            "POST /prompt with token {bad:?} should be 401: {body}"
        );

        let (code, body) = ws_upgrade(&url, header.as_deref());
        assert_eq!(
            code, 401,
            "GET /ws with token {bad:?} should be 401, not an upgrade: {body}"
        );

        let (code, body) = get(&url, "/no-such-route", header.as_deref());
        assert_eq!(
            code, 401,
            "an unknown path must still 401 before routing, not 404: {body}"
        );
    }

    let (code, body) = get(&url, "/", Some(&format!("Authorization: Bearer {TOKEN}")));
    assert_eq!(code, 200, "the real token should still work: {body}");

    let _ = child.kill();
    let _ = child.wait();
}

/// A duplicate `Authorization` header is a classic smuggling trick against
/// proxies that disagree about which one wins; this nails down that tenon's
/// own parser (first match in header order) never lets a wrong token ride
/// alongside a right one, whichever position the right one is in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_authorization_headers_use_the_first_one_and_never_bypass() {
    let Some(fixture) = fixture("authz-adv-dup") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--auth-token", TOKEN]);

    let request = format!(
        "GET / HTTP/1.1\r\nHost: {url}\r\nAuthorization: Bearer wrong-one\r\n\
         Authorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    let (code, body) = raw_request(&url, &request);
    assert_eq!(
        code, 401,
        "a wrong token before a right one must not authenticate: {body}"
    );

    let request = format!(
        "GET / HTTP/1.1\r\nHost: {url}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Authorization: Bearer wrong-one\r\nConnection: close\r\n\r\n"
    );
    let (code, body) = raw_request(&url, &request);
    assert_eq!(
        code, 200,
        "the first (right) header should still authenticate: {body}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// The `Authorization` scheme name is case-insensitive per RFC 7235, but
/// tenon's parser only recognises the exact strings `Bearer ` and
/// `bearer `. This documents the gap honestly: it fails *closed* (a
/// differently-cased scheme is simply not recognised as a bearer header, so
/// the request is treated as tokenless and 401s) rather than open, so it is
/// a spec-compliance gap, not a bypass -- and the same token still works via
/// `?token=`, proving the value itself was never the problem.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uppercase_bearer_scheme_is_not_recognised_but_fails_closed() {
    let Some(fixture) = fixture("authz-adv-case") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--auth-token", TOKEN]);

    let (code, body) = get(&url, "/", Some(&format!("Authorization: BEARER {TOKEN}")));
    assert_eq!(
        code, 401,
        "an all-caps BEARER scheme is not parsed as a bearer header: {body}"
    );

    let (code, body) = get(&url, &format!("/?token={TOKEN}"), None);
    assert_eq!(
        code, 200,
        "the same token over ?token= still authenticates: {body}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Extra whitespace around the header value must neither break a legitimate
/// client nor be usable to smuggle a different token past the compare.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extra_whitespace_in_the_header_value_still_authenticates_exactly() {
    let Some(fixture) = fixture("authz-adv-space") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--auth-token", TOKEN]);

    let (code, body) = get(
        &url,
        "/",
        Some(&format!("Authorization: Bearer  {TOKEN}  ")),
    );
    assert_eq!(
        code, 200,
        "surrounding whitespace around a correct token should still pass: {body}"
    );

    let (code, body) = get(
        &url,
        "/",
        Some(&format!("Authorization: Bearer {TOKEN}extra")),
    );
    assert_eq!(
        code, 401,
        "trailing garbage appended to the token must not authenticate: {body}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// The token is only ever read from the `Authorization` header or
/// `?token=`; a form-encoded body field is not a valid channel even for a
/// route that already reads other body fields (`POST /prompt` reads
/// `text`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_placed_only_in_the_post_body_is_rejected() {
    let Some(fixture) = fixture("authz-adv-body") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--auth-token", TOKEN]);

    let (code, body) = post_prompt(&url, None, &format!("text=hi&token={TOKEN}"));
    assert_eq!(
        code, 401,
        "a token embedded only in the POST body must not authenticate: {body}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// `--public` is documented as the explicit "skip the token" escape hatch,
/// applied to the whole serve surface (there is no per-app scoping yet --
/// that is P4.5 ingress). This confirms that flag really does open every
/// route including `/ws` to a tokenless client, so the severity of turning
/// it on is accurately understood: it is not "read-only UI," it is the
/// entire RPC surface unauthenticated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_flag_opens_every_route_including_ws_with_no_token() {
    let Some(fixture) = fixture("authz-adv-public") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;
    let (mut child, url) = serve(&fixture, &["--public"]);

    let (code, body) = get(&url, "/", None);
    assert_eq!(code, 200, "public GET / should need no token: {body}");

    let stream = tokio::net::TcpStream::connect(&url).await.expect("tcp");
    let request = format!("ws://{url}/ws");
    let outcome = tokio_tungstenite::client_async(request, stream).await;
    assert!(
        outcome.is_ok(),
        "public serve should let a tokenless client complete the /ws upgrade too"
    );

    let _ = child.kill();
    let _ = child.wait();
}
