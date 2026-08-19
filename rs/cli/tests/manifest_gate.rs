mod gate;

use gate::{release, Fixture};
use serde_json::{json, Value};
use std::time::Duration;

const NAME: &str = "manifest-gate";

/// The manifest half of P3.5 is about files on the host, not about the agent:
/// no container is needed to promote an LKG and verify it.
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

fn install(fixture: &Fixture, name: &str, version: &str, hash: &str) {
    let dir = fixture
        .home
        .join("plugins")
        .join(format!("{name}@{version}"));
    std::fs::create_dir_all(&dir).expect("plugin dir");
    let manifest = json!({
        "name": name,
        "version": version,
        "hash": hash,
        "cmd": "/bin/true",
        "args": [],
        "protocol": "wire/1",
    });
    std::fs::write(dir.join("manifest.json"), manifest.to_string()).expect("manifest");
}

fn lkg(fixture: &Fixture) -> Value {
    let body = std::fs::read_to_string(fixture.home.join("lkg/manifest.json")).expect("lkg");
    serde_json::from_str(&body).expect("manifest json")
}

/// The P3.5 manifest gate: a promotion pins config, profiles, the state copy
/// and every installed plugin; `tenon status --lkg` reports it; `tenon
/// rollback` verifies the hashes before restoring and refuses with what
/// differs when they moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lkg_manifest_is_written_at_promotion_and_checked_before_a_rollback() {
    let Some(release) = skip() else { return };
    let fixture = Fixture::new(NAME, release, CONFIG, HARNESS);
    install(&fixture, "echo", "1.0.0", "sha256:echo");
    fixture.start();
    assert!(
        fixture
            .await_status(Duration::from_secs(120), |status| status["nodes"]
                .as_array()
                .map(|nodes| nodes.iter().all(|node| node["registered"] == true))
                .unwrap_or(false))
            .await,
        "nodes never registered\n{}",
        fixture.log()
    );

    // a. every field of RFC section 10's manifest is there
    let manifest = lkg(&fixture);
    for key in [
        "config_hash",
        "profile_hash",
        "release_version",
        "state_copy",
    ] {
        assert!(!manifest[key].is_null(), "{key} missing: {manifest}");
    }
    assert_eq!(manifest["plugins"][0]["name"], "echo", "{manifest}");
    assert_eq!(manifest["plugins"][0]["hash"], "sha256:echo", "{manifest}");
    assert_eq!(manifest["state_copy"]["path"], "state.sqlite", "{manifest}");

    // b. tenon status --lkg reports it and verifies clean
    let (ok, out, err) = fixture.run(&["status", "--lkg"]);
    assert!(ok, "status --lkg failed: {out}{err}");
    assert!(out.contains("\"verified\": true"), "{out}");
    assert!(out.contains("release_version"), "{out}");

    let (ok, out, err) = fixture.run(&["stop"]);
    assert!(ok, "stop failed: {out}{err}");
    std::thread::sleep(Duration::from_millis(500));

    // c. a plugin whose hash moved is a refusal naming it
    install(&fixture, "echo", "1.0.0", "sha256:tampered");
    let (ok, out, err) = fixture.run(&["rollback"]);
    assert!(!ok, "rollback should have refused: {out}");
    assert!(err.contains("does not match"), "{err}");
    assert!(err.contains("plugin"), "{err}");
    let (ok, out, _err) = fixture.run(&["status", "--lkg"]);
    assert!(!ok, "status --lkg should report the drift: {out}");
    assert!(out.contains("\"verified\": false"), "{out}");

    // d. with the plugin back, a live config change is rolled back
    install(&fixture, "echo", "1.0.0", "sha256:echo");
    std::fs::write(fixture.home.join("config.yml"), "root_env: broken\n").expect("break config");
    let (ok, out, err) = fixture.run(&["rollback"]);
    assert!(ok, "rollback failed: {out}{err}");
    assert!(out.contains("config.yml"), "{out}");
    let restored = std::fs::read_to_string(fixture.home.join("config.yml")).expect("config");
    assert!(restored.contains("sandbox: none"), "{restored}");
    assert!(!restored.contains("broken"), "{restored}");
}
