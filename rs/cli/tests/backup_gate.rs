mod gate;

use gate::{fixture, skip, Fixture, Spec, BIN};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;
use tenon_harness::fake::{self, Say};

const NAME: &str = "backup-gate";

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 4\napproval: deny\n"
    )
}

fn sha256_file(path: &Path) -> String {
    let bytes = std::fs::read(path).expect("read file for hash");
    Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn copy_tree(from: &Path, into: &Path) {
    std::fs::create_dir_all(into).expect("mkdir");
    for entry in std::fs::read_dir(from).expect("read_dir") {
        let entry = entry.expect("entry");
        let target = into.join(entry.file_name());
        if entry.file_type().expect("file_type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

fn manifest_entry<'a>(manifest: &'a Value, rel: &str) -> &'a Value {
    manifest["files"]
        .as_array()
        .expect("files array")
        .iter()
        .find(|file| file["path"] == json!(rel))
        .unwrap_or_else(|| panic!("{rel} not listed in backup.json: {manifest}"))
}

/// P4.6: a live backup, its checksummed manifest, a refusal over a running base,
/// a refusal on a tampered file, and a restore into a fresh home whose base
/// replays the session the first home ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backup_restore_round_trips_a_session_and_refuses_bad_input() {
    let Some(release) = skip(NAME) else {
        return;
    };
    let server = fake::spawn(vec![Say::Text("remembered".to_string())])
        .await
        .expect("fake model");
    let source = fixture(
        NAME,
        release.clone(),
        "sandbox: oci\n",
        &harness(&server.base_url),
    );
    source.start();
    source.ready(Duration::from_secs(120)).await;

    // a fake-model turn, so state-root.sqlite has a real session log
    let (ok, out, err) = source.run(&["run", "remember this", "--timeout", "120"]);
    assert!(ok, "tenon run failed: {out}{err}\n{}", source.log());
    let session = source
        .events()
        .await
        .into_iter()
        .find_map(|event| event["data"]["session"].as_str().map(str::to_string))
        .expect("a session id in the log");

    // backup while base is running
    let dir = source.home.join("bak");
    let dir_str = dir.display().to_string();
    let (ok, out, err) = source.run(&["backup", &dir_str]);
    assert!(ok, "backup failed: {out}{err}\n{}", source.log());

    // backup.json lists the state files with checksums that match the copies
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("backup.json")).expect("read backup.json"),
    )
    .expect("parse backup.json");
    assert_eq!(manifest["envs"], json!(["root"]), "{manifest}");
    for rel in ["state.sqlite", "state-root.sqlite"] {
        let entry = manifest_entry(&manifest, rel);
        let on_disk = sha256_file(&dir.join(rel));
        assert_eq!(
            entry["sha256"],
            json!(on_disk),
            "{rel} sha256 drift: {entry}"
        );
        assert!(
            entry["bytes"].as_u64().unwrap_or(0) > 0,
            "{rel} empty: {entry}"
        );
    }

    // restore refuses over a running base
    let (ok, out, err) = source.run(&["restore", &dir_str]);
    assert!(!ok, "restore over a live base should refuse: {out}");
    assert!(
        format!("{out}{err}").contains("running"),
        "restore did not name the live base: {out}{err}"
    );

    // restore refuses a tampered backup, naming the file that differs
    let target = Fixture::open(
        BIN,
        release,
        Spec {
            name: "backup-restore",
            ..Spec::default()
        },
    );
    let tampered = target.home.join("tampered");
    copy_tree(&dir, &tampered);
    let victim = tampered.join("state-root.sqlite");
    let mut bytes = std::fs::read(&victim).expect("read state file");
    bytes[100] ^= 0xff;
    std::fs::write(&victim, &bytes).expect("tamper");
    let (ok, out, err) = target.run(&["restore", &tampered.display().to_string()]);
    assert!(!ok, "a tampered backup restored anyway: {out}{err}");
    assert!(
        format!("{out}{err}").contains("state-root.sqlite"),
        "the mismatch was not named: {out}{err}"
    );

    // the clean backup restores into the fresh home
    let (ok, out, err) = target.run(&["restore", &dir_str]);
    assert!(ok, "clean restore failed: {out}{err}");
    assert!(
        out.contains("state-root.sqlite"),
        "restore said nothing: {out}"
    );
    assert!(
        target.home.join("state-root.sqlite").is_file(),
        "the session state file was not put back"
    );

    // a base in the restored home replays the session history
    target.start();
    target.ready(Duration::from_secs(120)).await;
    let history = target
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

    source.run(&["stop"]);
    target.run(&["stop"]);
}
