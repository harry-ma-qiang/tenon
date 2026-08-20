mod gate;

use gate::{collect, fixture, skip};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use tenon_base::client::Client;
use tenon_harness::fake::{self, Say};

const NAME: &str = "replay-gate";

/// The pack must be pushed by the shutdown, not by the timer, so the pull
/// interval is longer than the test; `env_user` exercises the unprivileged
/// path of the per-env privilege drop.
const CONFIG: &str = "sandbox: oci\nenv_user: nobody\nworker:\n  pull_interval_ms: 600000\n";

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 6\napproval: deny\n"
    )
}

async fn gone(path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if !path.exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// The P3.5 exit-on-detach and replay gate: a turn that commits the workspace,
/// a detach that stops everything after pushing the pack and flushing the log,
/// and a second start that rebuilds the sandbox from the packs and answers
/// `session.history` for the session the first boot ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_detach_stops_everything_and_the_next_start_replays_the_workspace_and_the_log() {
    let Some(release) = skip(NAME) else { return };
    let server = fake::spawn(vec![]).await.expect("fake model");
    let fixture = fixture(NAME, release, CONFIG, &harness(&server.base_url));
    let (ok, out, err) = fixture.run(&["start", "--exit-on-detach"]);
    assert!(ok, "start failed: {out}{err}\n{}", fixture.log());
    fixture.ready(Duration::from_secs(180)).await;

    // a. env_user was asked for and could not be granted: base said so and carried on
    let privilege = fixture
        .base_events("env.privilege")
        .await
        .into_iter()
        .next()
        .expect("an env.privilege event");
    assert_eq!(privilege["data"]["env_user"], "nobody", "{privilege}");
    assert_eq!(privilege["data"]["dropping"], false, "{privilege}");

    // b. one attach holds the door open while a turn commits the workspace
    let mut watcher = Client::connect(&fixture.home.join("run/base.sock"))
        .await
        .expect("subscribe connection");
    watcher
        .call(
            "bus.subscribe",
            json!({"topics": ["session/**", "base/**"]}),
        )
        .await
        .expect("subscribe");

    server.say(vec![
        Say::Tool(
            "bash".to_string(),
            json!({"cmd": "echo replay-ok > kept.txt"}),
        ),
        Say::Tool("snapshot".to_string(), json!({"op": "commit"})),
        Say::Text("committed".to_string()),
    ]);
    let (ok, out, err) = collect(fixture.spawn(&["run", "write and commit", "--timeout", "180"]));
    assert!(ok, "tenon run failed: {out}{err}\n{}", fixture.log());

    let packs = fixture
        .rpc("snap.list", json!({"env": "root"}))
        .await
        .expect("snap.list");
    assert_eq!(
        packs["count"], 0,
        "the timer must not have pulled yet: {packs}"
    );
    let session = fixture
        .events()
        .await
        .into_iter()
        .find_map(|event| event["data"]["session"].as_str().map(str::to_string))
        .expect("a session id in the log");

    // an uncommitted file: what a replay is allowed to lose
    let workspace = fixture.home.join("envs/root/workspace");
    std::fs::write(workspace.join("stray.txt"), "not committed").expect("stray");

    // c. the last subscriber leaves: base pushes the pack, flushes and exits
    drop(watcher);
    assert!(
        gone(
            &fixture.home.join("run/base.ready"),
            Duration::from_secs(60)
        )
        .await,
        "base never exited on detach\n{}",
        fixture.log()
    );
    assert!(
        gone(&fixture.home.join("run/base.sock"), Duration::from_secs(30)).await,
        "the front door outlived base"
    );

    // d. a second start: fresh sandbox, workspace replayed from the packs
    let (ok, out, err) = fixture.run(&["start"]);
    assert!(ok, "second start failed: {out}{err}\n{}", fixture.log());
    fixture.ready(Duration::from_secs(180)).await;

    let packs = fixture
        .rpc("snap.list", json!({"env": "root"}))
        .await
        .expect("snap.list");
    assert!(
        packs["count"].as_i64().unwrap_or(0) >= 1,
        "the detach did not push the pack: {packs}"
    );
    let restored = fixture
        .await_status(Duration::from_secs(90), |_status| {
            workspace.join("kept.txt").is_file()
        })
        .await;
    assert!(restored, "kept.txt never came back\n{}", fixture.log());
    let kept = std::fs::read_to_string(workspace.join("kept.txt")).expect("kept.txt");
    assert!(kept.contains("replay-ok"), "{kept}");
    assert!(
        !workspace.join("stray.txt").exists(),
        "an uncommitted file survived the replay"
    );
    assert!(
        !fixture.base_events("env.restored").await.is_empty(),
        "no env.restored event\n{}",
        fixture.log()
    );

    // e. the session log survived: history and resume both answer for it
    let history = fixture
        .rpc(
            "session.history",
            json!({"env": "root", "session_id": session}),
        )
        .await
        .expect("session.history");
    let kinds: Vec<String> = history["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|event| event["kind"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(kinds.contains(&"user/message".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"turn/end".to_string()), "{kinds:?}");
    let resumed = fixture
        .rpc(
            "session.resume",
            json!({"env": "root", "session_id": session}),
        )
        .await
        .expect("session.resume");
    assert!(
        resumed["messages"].as_i64().unwrap_or(0) >= 2,
        "the fresh harness did not fold the log back: {resumed}"
    );
}
