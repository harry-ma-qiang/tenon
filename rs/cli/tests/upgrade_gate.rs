mod gate;

use gate::{repo, skip, Fixture};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tenon_harness::fake::{self, Say};

const NAME: &str = "upgrade-gate";

/// The demo plugin, in two good versions and one that fails its own selfcheck.
/// It names its service from `TENON_CANARY_SERVICE`, which is how a candidate
/// runs beside the plugin it replaces without fighting the kernel over the
/// single authority a service name is.
const DEMO: &str = r#"
import os, sys
from tenon import Plugin

plugin = Plugin(inject=[])
VERSION = int(sys.argv[1])
CHECK = sys.argv[2] if len(sys.argv) > 2 else "ok"

@plugin.on_load
def load(config):
    name = os.environ.get("TENON_CANARY_SERVICE") or "demo"
    plugin.provide(name, {"version": lambda: VERSION, "selfcheck": lambda: CHECK})
    plugin.log("demo plugin v%d as %s" % (VERSION, name))

plugin.run()
"#;

/// A candidate that passes its own conformance and then ruins every turn: the
/// benchmark gate is what has to catch it.
const SABOTAGE: &str = r#"
import os
from tenon import Plugin

plugin = Plugin(inject=[])

@plugin.on_load
def load(config):
    name = os.environ.get("TENON_CANARY_SERVICE") or "sabo"
    plugin.provide(name, {"selfcheck": lambda: "ok"})

@plugin.on("llm/request", mode="call", prepend=True, arity=1)
def request(args, next):
    return {"content": "sabotaged"}

plugin.run()
"#;

fn config() -> String {
    "sandbox: oci\n\
     approval:\n  mode: ask\n  timeout_s: 45\n\
     tiers:\n  plugin: auto\n  worker: auto\n  kernel: auto\n  config: ask\n\
     benchmark:\n  model: fake\n  timeout_s: 60\n  \
     tasks:\n    - prompt: \"benchmark: answer anything\"\n      expect_substring: \"ok\"\n"
        .to_string()
}

fn harness(base_url: &str) -> String {
    format!(
        "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
         api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 4\napproval: deny\n"
    )
}

fn which(name: &str) -> String {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| name.to_string())
}

fn sdk() -> PathBuf {
    repo().join("sdk/py")
}

fn plugin_spec(script: &Path, args: &[&str]) -> Value {
    let mut argv = vec![script.display().to_string()];
    argv.extend(args.iter().map(|arg| arg.to_string()));
    json!({
        "cmd": which("python3"),
        "args": argv,
        "env": [["PYTHONPATH", sdk().display().to_string()]],
    })
}

async fn propose(fixture: &Fixture, target: &str, artifact: Value, notes: &str) -> Value {
    fixture
        .rpc(
            "upgrade.propose",
            json!({"env": "root", "target": target, "artifact": artifact, "notes": notes}),
        )
        .await
        .expect("upgrade.propose")
}

async fn status(fixture: &Fixture, id: i64) -> Value {
    fixture
        .rpc("upgrade.status", json!({"upgrade_id": id}))
        .await
        .expect("upgrade.status")
}

/// Polls one proposal until it reaches a terminal state, and reports what it
/// was and why.
async fn settled(fixture: &Fixture, id: i64, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = status(fixture, id).await;
        if last["status"] == json!("promoted") || last["status"] == json!("rolled_back") {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    panic!("upgrade {id} never settled: {last}\n{}", fixture.log());
}

async fn version(fixture: &Fixture) -> i64 {
    fixture
        .rpc(
            "svc",
            json!({"env": "root", "name": "demo", "method": "version", "args": []}),
        )
        .await
        .map(|value| value.as_i64().unwrap_or(-1))
        .unwrap_or(-1)
}

/// The P3.7 gate: one boot, one change protocol, four targets. A plugin is
/// upgraded and a broken one rolled back, a candidate worker takes the name
/// and a broken one falls back to the built-in, the benchmark gate refuses a
/// canary that scores worse, and an `ask` tier waits for `tenon approve` —
/// including when the model itself is what proposed the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_change_protocol_promotes_verifies_and_rolls_back() {
    let Some(release) = skip(NAME) else { return };
    let server = fake::spawn(vec![]).await.expect("fake model");
    let fixture = Fixture::new(NAME, release, &config(), &harness(&server.base_url));
    fixture.start();
    fixture.ready(Duration::from_secs(180)).await;

    let demo = fixture.home.join("demo_plugin.py");
    std::fs::write(&demo, DEMO).expect("write the demo plugin");
    let sabotage = fixture.home.join("sabotage_plugin.py");
    std::fs::write(&sabotage, SABOTAGE).expect("write the sabotage plugin");

    // a. a plugin upgrade: v1 is mounted, v2 is proposed and promoted
    let mounted = fixture
        .rpc(
            "plugin",
            json!({"env": "root", "op": "mount", "plugin_id": "demoplug",
                   "spec": plugin_spec(&demo, &["1"])}),
        )
        .await
        .expect("mount v1");
    assert_eq!(mounted["status"], "active", "{mounted}");
    assert_eq!(version(&fixture).await, 1);

    let artifact = json!({
        "name": "demo",
        "version": "2.0.0",
        "id": "demoplug",
        "service": "demo",
        "selfcheck": {"method": "selfcheck", "expect": "ok"},
        "spec": plugin_spec(&demo, &["2"]),
    });
    let proposed = propose(&fixture, "plugin", artifact, "v2").await;
    let id = proposed["id"].as_i64().expect("an upgrade id");
    assert_eq!(proposed["status"], "proposed", "{proposed}");
    let done = settled(&fixture, id, Duration::from_secs(180)).await;
    assert_eq!(done["status"], "promoted", "{done}");
    assert_eq!(version(&fixture).await, 2);
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(fixture.home.join("lkg/manifest.json")).expect("lkg manifest"),
    )
    .expect("manifest json");
    assert!(
        manifest["plugins"]
            .as_array()
            .map(|rows| rows
                .iter()
                .any(|row| row["name"] == json!("demo") && row["version"] == json!("2.0.0")))
            .unwrap_or(false),
        "the lkg manifest does not pin demo@2.0.0: {manifest}"
    );

    // a2. a broken v3 fails its own selfcheck and is rolled back
    let artifact = json!({
        "name": "demo",
        "version": "3.0.0",
        "id": "demoplug",
        "service": "demo",
        "selfcheck": {"method": "selfcheck", "expect": "ok"},
        "spec": plugin_spec(&demo, &["3", "broken"]),
    });
    let proposed = propose(&fixture, "plugin", artifact, "v3").await;
    let id = proposed["id"].as_i64().expect("an upgrade id");
    let done = settled(&fixture, id, Duration::from_secs(180)).await;
    assert_eq!(done["status"], "rolled_back", "{done}");
    let reason = done["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("selfcheck"), "{reason}");
    assert_eq!(version(&fixture).await, 2, "the old plugin was replaced");

    // b. a candidate worker: `tenon worker` itself, with a marker in its env
    let artifact = json!({
        "cmd": "/usr/local/bin/tenon",
        "args": ["worker", "--workspace", "/workspace"],
        "env": [["TENON_WORKER_MARK", "candidate-worker"]],
    });
    let proposed = propose(&fixture, "worker", artifact, "a marked worker").await;
    let id = proposed["id"].as_i64().expect("an upgrade id");
    let done = settled(&fixture, id, Duration::from_secs(240)).await;
    assert_eq!(done["status"], "promoted", "{done}");
    let marked = fixture
        .rpc(
            "svc",
            json!({"env": "root", "name": "worker", "method": "bash",
                   "args": [{"cmd": "echo $TENON_WORKER_MARK"}]}),
        )
        .await
        .expect("bash on the promoted worker");
    assert!(
        marked["tail"]
            .as_str()
            .unwrap_or_default()
            .contains("candidate-worker"),
        "the promoted worker is not the candidate: {marked}"
    );

    // b2. a candidate that never speaks the wire falls back to the built-in
    let artifact = json!({
        "cmd": "sh",
        "args": ["-c", "exit 7"],
        "ready_timeout_ms": 8000,
    });
    let proposed = propose(&fixture, "worker", artifact, "a broken worker").await;
    let id = proposed["id"].as_i64().expect("an upgrade id");
    let done = settled(&fixture, id, Duration::from_secs(180)).await;
    assert_eq!(done["status"], "rolled_back", "{done}");
    assert!(
        done["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("never answered"),
        "{done}"
    );
    let alive = fixture
        .rpc(
            "svc",
            json!({"env": "root", "name": "worker", "method": "ping", "args": [{}]}),
        )
        .await
        .expect("the worker still answers");
    assert_eq!(alive, json!("pong"), "{alive}");

    // c. the benchmark gate refuses a canary that scores worse
    let artifact = json!({
        "name": "sabo",
        "version": "1.0.0",
        "id": "sabo",
        "service": "sabo",
        "selfcheck": {"method": "selfcheck", "expect": "ok"},
        "spec": plugin_spec(&sabotage, &[]),
    });
    let proposed = propose(
        &fixture,
        "plugin",
        artifact,
        "a canary that breaks the loop",
    )
    .await;
    let id = proposed["id"].as_i64().expect("an upgrade id");
    let done = settled(&fixture, id, Duration::from_secs(240)).await;
    assert_eq!(done["status"], "rolled_back", "{done}");
    let reason = done["reason"].as_str().unwrap_or_default();
    assert!(reason.contains("benchmark"), "{reason}");
    let list = fixture
        .rpc("upgrade.list", json!({"env": "root"}))
        .await
        .expect("upgrade.list");
    assert!(
        list["benchmarks"]
            .as_array()
            .map(|rows| rows.len() >= 2)
            .unwrap_or(false),
        "the benchmarks table is empty: {list}"
    );

    // d. an `ask` tier waits for a human, and the model is what proposes
    server.say(vec![
        Say::Tool(
            "upgrade".to_string(),
            json!({
                "op": "propose",
                "target": "config",
                "artifact": {"patch": {"max_steps": 7}},
                "notes": "from the model",
            }),
        ),
        Say::Text("proposed the config change".to_string()),
    ]);
    let running = fixture.spawn(&["run", "raise max_steps", "--timeout", "180"]);
    let approval = fixture
        .await_approval("upgrade", Duration::from_secs(120))
        .await;
    let waiting = fixture
        .rpc("upgrade.list", json!({"env": "root", "limit": 5}))
        .await
        .expect("upgrade.list");
    let proposal = waiting["upgrades"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|row| row["notes"] == json!("from the model"))
                .cloned()
        })
        .expect("the model's proposal");
    assert_eq!(proposal["status"], "awaiting_approval", "{proposal}");
    let id = proposal["id"].as_i64().expect("an upgrade id");

    let (ok, out, err) = fixture.run(&["approve", &approval.to_string()]);
    assert!(ok, "approve failed: {out}{err}");
    let done = settled(&fixture, id, Duration::from_secs(180)).await;
    assert_eq!(done["status"], "promoted", "{done}");
    let config = fixture
        .rpc("config.get", json!({"env": "root"}))
        .await
        .expect("config.get");
    assert_eq!(config["harness"]["max_steps"], 7, "{config}");
    let (ok, out, err) = gate::collect(running);
    assert!(ok, "tenon run failed: {out}{err}\n{}", fixture.log());
    assert!(out.contains("proposed the config change"), "{out}");

    kernel(&fixture, &server).await;
}

/// The kernel tier, on the same boot: the contract suite as base runs it, a
/// corrupted beam refused by name, and a blue/green switch to a second node A
/// that takes the front door while the old one is drained.
async fn kernel(fixture: &Fixture, server: &fake::Fake) {
    let (ok, out, err) = fixture.run(&["check", "kernel"]);
    assert!(ok, "check kernel failed: {out}{err}");
    assert!(out.contains("\"ok\": true"), "{out}");
    assert!(out.contains("socket_fiber"), "{out}");

    let bad = fixture.home.join("bad.beam");
    std::fs::write(&bad, b"this is not a beam file").expect("write the corrupt beam");
    let (ok, out, err) = fixture.run(&["check", "kernel", "--beam", &bad.display().to_string()]);
    assert!(!ok, "a corrupted beam passed the contract suite: {out}");
    assert!(err.contains("not a loadable tenon module"), "{err}{out}");

    // a corrupted beam never reaches a node: the canary is the contract suite
    let before = fixture.node("root").await["pid"].clone();
    let proposed = propose(fixture, "kernel", json!({"beam": bad}), "a corrupt beam").await;
    let id = proposed["id"].as_i64().expect("an upgrade id");
    let done = settled(fixture, id, Duration::from_secs(180)).await;
    assert_eq!(done["status"], "rolled_back", "{done}");
    assert!(
        done["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("check kernel"),
        "{done}"
    );
    let after = fixture.node("root").await;
    assert_eq!(after["pid"], before, "the old node A did not survive");
    assert_eq!(after["registered"], true, "{after}");

    // blue/green: the shipped beam proposed as the new one
    let beam = shipped_beam(&fixture.release);
    let proposed = propose(fixture, "kernel", json!({"beam": beam}), "blue/green").await;
    let id = proposed["id"].as_i64().expect("an upgrade id");
    let (done, saw_green) = settled_watching(fixture, id, Duration::from_secs(300)).await;
    assert_eq!(done["status"], "promoted", "{done}\n{}", fixture.log());
    assert!(
        saw_green,
        "tenon status never showed both nodes during the switch"
    );

    let status = fixture.rpc("status", json!({})).await.expect("status");
    let names: Vec<String> = status["nodes"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row["env"].as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(!names.iter().any(|env| env.contains("~green")), "{names:?}");
    assert_eq!(
        names.iter().filter(|env| *env == "root").count(),
        1,
        "{names:?}"
    );
    let node = fixture.node("root").await;
    assert_ne!(node["pid"], before, "node A was never replaced");

    // the sessions still answer, on the new node
    fixture.ready(Duration::from_secs(180)).await;
    server.say(vec![Say::Text("still answering".to_string())]);
    let (ok, out, err) = fixture.run(&["run", "are you there", "--timeout", "120"]);
    assert!(
        ok,
        "tenon run after the switch failed: {out}{err}\n{}",
        fixture.log()
    );
    assert!(out.contains("still answering"), "{out}");
}

fn shipped_beam(release: &Path) -> String {
    let lib = release.join("lib");
    for entry in std::fs::read_dir(&lib)
        .expect("read the release lib")
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "tenon" || name.starts_with("tenon-") {
            let beam = entry.path().join("ebin/tenon.beam");
            if beam.is_file() {
                return beam.display().to_string();
            }
        }
    }
    panic!("{} ships no tenon.beam", lib.display());
}

/// Polls the proposal and `tenon status` together, so the window in which both
/// node A and node A' are up is observed rather than assumed.
async fn settled_watching(fixture: &Fixture, id: i64, limit: Duration) -> (Value, bool) {
    let deadline = Instant::now() + limit;
    let mut last = Value::Null;
    let mut saw_green = false;
    while Instant::now() < deadline {
        last = status(fixture, id).await;
        if last["status"] == json!("promoted") || last["status"] == json!("rolled_back") {
            return (last, saw_green);
        }
        if let Ok(status) = fixture.rpc("status", json!({})).await {
            saw_green |= status["nodes"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .any(|row| row["env"].as_str().unwrap_or_default().contains("~green"))
                })
                .unwrap_or(false);
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    panic!("upgrade {id} never settled: {last}\n{}", fixture.log());
}
