mod gate;

use gate::{release, Fixture};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NAME: &str = "contract-gate";

/// No container: the contract is about base and the harness, and a failed
/// worker costs the agent its hands, not its loop.
const CONFIG: &str = "sandbox: none\n";

const HARNESS: &str = "llm:\n  provider: openai\n  base_url: http://127.0.0.1:1\n  \
model: fake-model\n  api_key_env: TENON_TEST_NO_KEY\nmax_steps: 2\napproval: deny\n";

fn skip() -> Option<std::path::PathBuf> {
    match release() {
        Some(dir) => Some(dir),
        None => {
            println!("skipping {NAME}: no beam release, set TENON_RELEASE_DIR");
            None
        }
    }
}

/// A health endpoint of the `http` kind: what a runtime that is not a BEAM
/// plugin — DSH behind the bridge — declares instead of a service method.
async fn health_endpoint(ok: bool) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut stream, _peer)) = listener.accept().await {
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request).await;
            let body = match ok {
                true => "HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok",
                false => "HTTP/1.0 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            };
            let _ = stream.write_all(body.as_bytes()).await;
        }
    });
    format!("http://127.0.0.1:{port}/health")
}

async fn await_runtime(fixture: &Fixture, name: &str, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = fixture.node("root").await;
        if last["runtime"]["manifest"]["name"] == name {
            return last["runtime"].clone();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    panic!("runtime {name} never registered: {last}\n{}", fixture.log());
}

fn register(token: &str, manifest: Value, health: Value) -> Value {
    json!({
        "env": "root",
        "token": token,
        "manifest": manifest,
        "health": health,
        "channels": {"events": "events.append", "approvals": "approval.request"},
    })
}

/// The P3.5 runtime-contract gate: base registers its own default runtime,
/// an outside runtime registers with the env's token and is probed through
/// the health target it declared, and every way of not meeting the contract
/// is refused with the reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runtime_registers_only_when_it_meets_the_contract_and_answers_its_probe() {
    let Some(release) = skip() else { return };
    let fixture = Fixture::new(NAME, release, CONFIG, HARNESS);
    fixture.start();

    // a. base registers the default runtime on behalf of its own env
    let runtime = await_runtime(&fixture, "tenon-default", Duration::from_secs(120)).await;
    assert_eq!(runtime["health"]["target"], "loop.ping", "{runtime}");
    assert_eq!(runtime["channels"]["events"], "events.append", "{runtime}");
    assert_eq!(runtime["contract"], "1", "{runtime}");
    assert_eq!(
        runtime["manifest"]["hash"]
            .as_str()
            .unwrap_or_default()
            .len(),
        64,
        "the default manifest carries a sha256: {runtime}"
    );
    let registered = fixture.of_kind("runtime.register").await;
    assert!(!registered.is_empty(), "no runtime.register event");

    // b. an outside runtime registers with the env's runtime token
    let token = std::fs::read_to_string(fixture.home.join("run/rt-root.token")).expect("token");
    let answer = fixture
        .rpc(
            "runtime.register",
            register(
                &token,
                json!({"name": "dsh", "version": "0.9.1", "hash": "sha256:beef"}),
                json!({"kind": "rpc", "target": "loop.ping"}),
            ),
        )
        .await
        .expect("runtime.register");
    assert_eq!(answer["manifest"]["name"], "dsh", "{answer}");
    let node = fixture.node("root").await;
    assert_eq!(node["runtime"]["manifest"]["version"], "0.9.1", "{node}");

    // c. the http health kind, which is how a bridged runtime declares itself
    let target = health_endpoint(true).await;
    let answer = fixture
        .rpc(
            "runtime.register",
            register(
                &token,
                json!({"name": "dsh-bridge", "version": "2.0.0", "hash": "sha256:cafe"}),
                json!({"kind": "http", "target": target}),
            ),
        )
        .await
        .expect("runtime.register");
    assert_eq!(answer["manifest"]["name"], "dsh-bridge", "{answer}");

    // d. a health endpoint that refuses is not a runtime base will supervise
    let target = health_endpoint(false).await;
    let error = fixture
        .rpc(
            "runtime.register",
            register(
                &token,
                json!({"name": "sick", "version": "1", "hash": "h"}),
                json!({"kind": "http", "target": target}),
            ),
        )
        .await
        .expect_err("a failing probe is a refusal");
    assert!(error.contains("health probe failed"), "{error}");
    assert!(error.contains("503"), "{error}");

    // e. the wrong token, an incomplete manifest and a bad health kind
    let error = fixture
        .rpc(
            "runtime.register",
            register(
                "not-the-token",
                json!({"name": "thief", "version": "1", "hash": "h"}),
                json!({"kind": "rpc", "target": "loop.ping"}),
            ),
        )
        .await
        .expect_err("a forged token is refused");
    assert_eq!(error, "unauthorized", "{error}");
    let error = fixture
        .rpc(
            "runtime.register",
            register(
                &token,
                json!({"name": "half", "hash": "h"}),
                json!({"kind": "rpc", "target": "loop.ping"}),
            ),
        )
        .await
        .expect_err("an incomplete manifest is refused");
    assert!(error.contains("manifest.version"), "{error}");
    let error = fixture
        .rpc(
            "runtime.register",
            register(
                &token,
                json!({"name": "odd", "version": "1", "hash": "h"}),
                json!({"kind": "smoke-signal", "target": "x.y"}),
            ),
        )
        .await
        .expect_err("an unknown health kind is refused");
    assert!(error.contains("rpc or http"), "{error}");

    // f. every refusal is in that env's log, and the last good runtime stands
    let refused = fixture.of_kind("runtime.refused").await;
    assert!(refused.len() >= 3, "refusals not logged: {refused:?}");
    let node = fixture.node("root").await;
    assert_eq!(node["runtime"]["manifest"]["name"], "dsh-bridge", "{node}");
}
