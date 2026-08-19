mod gate;

use gate::{collect, skip, Fixture};
use serde_json::json;
use std::time::Duration;
use tenon_harness::fake::{self, Say};

const NAME: &str = "approvals-gate";
const TIMEOUT_S: u64 = 8;

fn config() -> String {
    format!("sandbox: oci\napproval:\n  mode: ask\n  timeout_s: {TIMEOUT_S}\n")
}

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 4\napproval: ask\n\
         gated_tools:\n  - bash\n"
    )
}

/// The P3.5 approvals gate: one boot, three verdicts. A gated tool call blocks
/// in base's queue, `tenon approvals` shows it, and `tenon approve` releases
/// it; a denial and a timeout come back to the model as tool results instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gated_tool_blocks_until_a_human_approves_denies_or_the_row_expires() {
    let Some(release) = skip(NAME) else { return };
    let server = fake::spawn(vec![]).await.expect("fake model");
    let fixture = Fixture::new(NAME, release, &config(), &harness(&server.base_url));
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    // a. approve: the tool runs and the model sees its output
    server.say(vec![
        Say::Tool("bash".to_string(), json!({"cmd": "echo tenon-ok"})),
        Say::Text("the output was tenon-ok".to_string()),
    ]);
    let running = fixture.spawn(&["run", "run echo with bash", "--timeout", "120"]);
    let id = fixture
        .await_approval("tool bash", Duration::from_secs(40))
        .await;
    let (ok, out, err) = fixture.run(&["approve", &id.to_string(), "--note", "go ahead"]);
    assert!(ok, "approve failed: {out}{err}");
    assert!(out.contains("approved"), "{out}");
    let (ok, out, err) = collect(running);
    assert!(ok, "tenon run failed: {out}{err}\n{}", fixture.log());
    let results = fixture.of_kind("tool/result").await;
    let result = results.last().expect("a tool result");
    assert_eq!(result["ok"], true, "{result}");
    assert!(
        result["text"]
            .as_str()
            .unwrap_or_default()
            .contains("tenon-ok"),
        "the approved call never ran: {result}"
    );
    let answered = fixture
        .rpc("approval.list", json!({"status": "approved"}))
        .await
        .expect("approval.list");
    assert_eq!(answered["approvals"][0]["note"], "go ahead", "{answered}");

    // b. deny: the model gets the reason as the tool result, the turn survives
    let before = fixture.of_kind("tool/result").await.len();
    server.say(vec![
        Say::Tool("bash".to_string(), json!({"cmd": "echo second"})),
        Say::Text("that was refused".to_string()),
    ]);
    let running = fixture.spawn(&["run", "run echo again", "--timeout", "120"]);
    let id = fixture
        .await_approval("echo second", Duration::from_secs(40))
        .await;
    let (ok, out, err) = fixture.run(&["approve", &id.to_string(), "--deny"]);
    assert!(ok, "deny failed: {out}{err}");
    assert!(out.contains("denied"), "{out}");
    let (ok, out, err) = collect(running);
    assert!(ok, "tenon run failed: {out}{err}");
    let results = fixture.of_kind("tool/result").await;
    assert!(results.len() > before, "no new tool result");
    let denied = results.last().expect("a tool result");
    assert_eq!(denied["denied"], true, "{denied}");
    assert!(
        denied["text"]
            .as_str()
            .unwrap_or_default()
            .contains("denied"),
        "{denied}"
    );

    // c. timeout: nobody answers, the row expires and the call comes back
    let before = fixture.of_kind("tool/result").await.len();
    server.say(vec![
        Say::Tool("bash".to_string(), json!({"cmd": "echo third"})),
        Say::Text("nobody answered".to_string()),
    ]);
    let running = fixture.spawn(&["run", "run echo a third time", "--timeout", "120"]);
    let id = fixture
        .await_approval("echo third", Duration::from_secs(40))
        .await;
    let (ok, out, err) = collect(running);
    assert!(ok, "tenon run failed: {out}{err}");
    let results = fixture.of_kind("tool/result").await;
    assert!(results.len() > before, "no new tool result");
    let expired = results.last().expect("a tool result");
    assert_eq!(expired["denied"], true, "{expired}");
    assert!(
        expired["text"]
            .as_str()
            .unwrap_or_default()
            .contains("expired"),
        "{expired}"
    );
    let row = fixture
        .rpc("approval.list", json!({"status": "expired"}))
        .await
        .expect("approval.list");
    assert!(
        row["approvals"]
            .as_array()
            .map(|rows| rows.iter().any(|row| row["id"] == json!(id)))
            .unwrap_or(false),
        "{row}"
    );

    // d. a host-affecting RPC is gated too: workspace push-out
    let sock = fixture.home.join("run/base.sock");
    let target = fixture.home.join("export.pack").display().to_string();
    let pushing = tokio::spawn(async move {
        let mut client = tenon_base::client::Client::connect(&sock)
            .await
            .expect("connect");
        client
            .call("snap.export", json!({"env": "root", "path": target}))
            .await
            .map_err(|error| error.to_string())
    });
    let id = fixture
        .await_approval("snap.export", Duration::from_secs(30))
        .await;
    let (ok, out, err) = fixture.run(&["approve", &id.to_string(), "--deny"]);
    assert!(ok, "deny failed: {out}{err}");
    let refused = pushing.await.expect("join").expect_err("a denied export");
    assert!(refused.contains("snap.export needs a human"), "{refused}");
    assert!(!fixture.home.join("export.pack").exists());

    // e. the queue is empty again and the event log carries every verdict
    let pending = fixture
        .rpc("approval.list", json!({"status": "pending"}))
        .await
        .expect("approval.list");
    assert_eq!(pending["count"], 0, "{pending}");
    let kinds: Vec<String> = fixture
        .events()
        .await
        .iter()
        .map(|event| event["kind"].as_str().unwrap_or_default().to_string())
        .collect();
    for wanted in ["approval.pending", "approval.decided", "approval.expired"] {
        assert!(
            kinds.contains(&wanted.to_string()),
            "{wanted} not in the log: {kinds:?}"
        );
    }
}
