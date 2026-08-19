mod gate;

use gate::{plain, skip};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const NAME: &str = "worker-boot";

#[tokio::test]
async fn the_env_boots_a_worker_pulls_its_packs_and_restores_them_on_reset() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let fixture = plain(
        NAME,
        release,
        "sandbox: oci\nworker:\n  pull_interval_ms: 2000\n",
    );
    fixture.start();

    let root = fixture.worker_ready("root", Duration::from_secs(90)).await;
    assert_eq!(root["sandbox"]["backend"], "oci", "{root}");
    assert!(root["worker"]["pid"].as_i64().unwrap_or(0) > 0, "{root}");

    let echoed = fixture
        .tool("root", "bash", json!({"cmd": "echo from-the-sandbox"}))
        .await
        .expect("bash");
    assert_eq!(echoed["status"], 0, "{echoed}");
    assert!(
        echoed["tail"]
            .as_str()
            .unwrap_or_default()
            .contains("from-the-sandbox"),
        "{echoed}"
    );

    fixture
        .tool(
            "root",
            "fs.write",
            json!({"path": "keep.txt", "content": "snapshotted\n"}),
        )
        .await
        .expect("fs.write");
    let committed = fixture
        .tool("root", "snap.commit", json!({"label": "keep"}))
        .await
        .expect("snap.commit");
    let step = committed["step"].as_i64().expect("step");

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut listed = Value::Null;
    while Instant::now() < deadline {
        listed = fixture
            .rpc("snap.list", json!({"env": "root"}))
            .await
            .expect("snap.list");
        if listed["count"].as_i64().unwrap_or(0) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        listed["count"].as_i64().unwrap_or(0) > 0,
        "the timer never pulled a pack: {listed}"
    );
    assert_eq!(listed["packs"][0]["step"], step, "{listed}");
    assert!(fixture.home.join("state-root.sqlite").is_file());

    let again = fixture
        .rpc("snap.pull", json!({"env": "root"}))
        .await
        .expect("snap.pull");
    assert_eq!(again["pulled"], 0, "a second pull re-sent a pack: {again}");

    fixture
        .tool(
            "root",
            "fs.write",
            json!({"path": "dirty.txt", "content": "never committed\n"}),
        )
        .await
        .expect("fs.write");
    assert!(fixture.workspace().join("dirty.txt").is_file());

    let (ok, text) = fixture.run_text(&["reset"]);
    assert!(ok, "reset failed: {text}");
    fixture.worker_ready("root", Duration::from_secs(120)).await;

    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline && !fixture.workspace().join("keep.txt").is_file() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        fixture.workspace().join("keep.txt").is_file(),
        "the snapshot was not replayed into the fresh workspace\n{}",
        fixture.log()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace().join("keep.txt")).unwrap(),
        "snapshotted\n"
    );
    assert!(
        !fixture.workspace().join("dirty.txt").is_file(),
        "an uncommitted file survived the reset"
    );
}
