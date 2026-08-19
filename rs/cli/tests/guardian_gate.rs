mod gate;

use gate::{skip, Fixture};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

const NAME: &str = "guardian-gate";
const PROBE_TIMEOUT_MS: u64 = 2000;

fn config(good: &str, bad: &str) -> String {
    format!(
        "sandbox: oci\nguardian:\n  interval_ms: 500\n  failures: 2\n  \
         probe_timeout_ms: {PROBE_TIMEOUT_MS}\n\
         probes:\n  extra:\n    - file: ok.sh\n      sha256: {good}\n    \
         - file: tampered.sh\n      sha256: {bad}\n"
    )
}

const HARNESS: &str = "llm:\n  provider: openai\n  base_url: http://127.0.0.1:1\n  \
model: fake-model\n  api_key_env: TENON_TEST_NO_KEY\nmax_steps: 2\napproval: deny\n";

fn write_probe(fixture: &Fixture, name: &str, body: &str) -> String {
    let dir = fixture.home.join("probes");
    std::fs::create_dir_all(&dir).expect("probes dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write probe");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let sum = Sha256::digest(body.as_bytes());
    sum.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn base_events(fixture: &Fixture, kind: &str) -> Vec<Value> {
    fixture
        .rpc("events.tail", json!({"env": "base", "limit": 5000}))
        .await
        .expect("events.tail base")["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|event| event["kind"] == kind)
        .collect()
}

async fn await_event(fixture: &Fixture, kind: &str, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(row) = fixture.of_kind(kind).await.into_iter().next() {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    panic!("no {kind} event in time\n{}", fixture.log());
}

/// The P3.5 guardian gate: the core probe set catches a wedged harness and
/// asks base to reset the env, base logs which probes failed and the env
/// comes back; and an extra probe reaches the guardian only when base's own
/// config carries its sha256.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wedged_env_is_reset_by_the_probes_and_only_signed_extra_probes_load() {
    let Some(release) = skip(NAME) else { return };
    let fixture = Fixture::new(NAME, release, "sandbox: oci\n", HARNESS);
    let good = write_probe(&fixture, "ok.sh", "#!/bin/sh\nexit 0\n");
    write_probe(&fixture, "tampered.sh", "#!/bin/sh\nexit 0\n");
    std::fs::write(fixture.home.join("config.yml"), config(&good, "0bad0")).unwrap();
    fixture.start();
    let node = fixture.ready(Duration::from_secs(180)).await;

    // a. the signed probe loaded, the one whose hash does not match did not
    let loaded = base_events(&fixture, "probes.loaded").await;
    assert_eq!(
        loaded.first().map(|row| row["data"]["count"].clone()),
        Some(json!(1)),
        "{loaded:?}"
    );
    let rejected = base_events(&fixture, "probes.rejected").await;
    let reason = rejected
        .first()
        .map(|row| {
            row["data"]["reason"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    assert!(reason.contains("sha256 is"), "{rejected:?}");
    assert_eq!(rejected[0]["data"]["file"], "tampered.sh", "{rejected:?}");

    // b. a frozen harness answers no probe, so the guardian resets the env
    let pid = node["harness"]["pid"].as_i64().expect("harness pid") as i32;
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let reset = await_event(&fixture, "guardian.reset", Duration::from_secs(60)).await;
    let probes: Vec<String> = reset["probes"]
        .as_array()
        .expect("probe names")
        .iter()
        .map(|name| name.as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        probes.contains(&"harness".to_string()) || probes.contains(&"wedged".to_string()),
        "the reset does not name the failing probe: {probes:?}"
    );

    // c. base performed it: the env is back with a fresh harness
    let back = fixture.ready(Duration::from_secs(180)).await;
    assert_ne!(back["harness"]["pid"], node["harness"]["pid"], "{back}");
    assert_eq!(back["registered"], true, "{back}");
}
