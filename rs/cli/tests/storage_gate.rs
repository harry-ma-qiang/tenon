use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tenon_base::client::Client;
use tenon_harness::fake::{self, Fake, Say};

const BIN: &str = env!("CARGO_BIN_EXE_tenon");
const NAME: &str = "storage-gate";
const SPEW: &str = "python3 -c \"print('x' * 20000)\"";

/// A small window so the gate can watch the retention policy bite: keep the
/// newest five snapshot steps, one milestone every tenth, and only the last 50
/// events, which is what makes the blobs of older tool results collectable.
const CONFIG: &str = "sandbox: oci\n\
worker:\n  pull_interval_ms: 60000\n\
retention:\n  keep_steps: 5\n  milestone_every: 10\n  keep_events: 50\n  blob_grace_ms: 0\n";

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn release() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TENON_RELEASE_DIR") {
        let dir = PathBuf::from(dir);
        return dir.join("bin/tenon_beam").is_file().then_some(dir);
    }
    let dir = repo().join("beam/_build/prod/rel/tenon_beam");
    dir.join("bin/tenon_beam").is_file().then_some(dir)
}

fn oci_available() -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .any(|dir| dir.join("podman").is_file() || dir.join("docker").is_file())
        })
        .unwrap_or(false)
}

fn skip() -> Option<PathBuf> {
    if !oci_available() {
        println!("skipping {NAME}: neither podman nor docker found in PATH");
        return None;
    }
    match release() {
        Some(dir) => Some(dir),
        None => {
            println!(
                "skipping {NAME}: no beam release. Build it with \
                 `cd beam && MIX_ENV=prod mix release` or set TENON_RELEASE_DIR"
            );
            None
        }
    }
}

struct Fixture {
    home: PathBuf,
    release: PathBuf,
}

impl Fixture {
    fn new(release: PathBuf, base_url: &str) -> Self {
        let home = std::env::temp_dir().join(format!("tenon-it-{}-{NAME}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("profiles/root")).unwrap();
        std::fs::write(home.join("config.yml"), CONFIG).unwrap();
        std::fs::write(
            home.join("profiles/root/harness.yml"),
            format!(
                "llm:\n  provider: openai\n  base_url: {base_url}\n  model: fake-model\n  \
                 api_key_env: TENON_TEST_NO_KEY\n  retry_base_ms: 20\nmax_steps: 4\napproval: deny\n"
            ),
        )
        .unwrap();
        Self { home, release }
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let output = Command::new(BIN)
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .env("TENON_RELEASE_DIR", &self.release)
            .output()
            .expect("run tenon");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    fn start(&self) {
        let (ok, out, err) = self.run(&["start"]);
        assert!(ok, "start failed: {out}{err}\n{}", self.log());
    }

    fn log(&self) -> String {
        ["base", "guardian", "root", "harness-root"]
            .iter()
            .map(|name| {
                let path = self.home.join(format!("run/{name}.log"));
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                format!("--- {name}.log\n{body}")
            })
            .collect()
    }

    async fn client(&self) -> Client {
        Client::connect(&self.home.join("run/base.sock"))
            .await
            .expect("connect to base")
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        self.client()
            .await
            .call(method, params)
            .await
            .map_err(|error| error.to_string())
    }

    async fn node(&self, env: &str) -> Value {
        self.rpc("status", json!({})).await.expect("status")["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|node| node["env"] == env)
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn ready(&self, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        let mut last = Value::Null;
        while Instant::now() < deadline {
            last = self.node("root").await;
            if last["worker"]["state"] == "ready" && last["harness"]["state"] == "ready" {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        panic!("root never became ready: {last}\n{}", self.log());
    }

    async fn tool(&self, method: &str, params: Value) -> Result<Value, String> {
        self.rpc(
            "svc",
            json!({"env": "root", "name": "worker", "method": method, "args": [params]}),
        )
        .await
    }

    async fn events(&self) -> Vec<Value> {
        self.rpc("events.tail", json!({"env": "root", "limit": 5000}))
            .await
            .expect("events.tail")["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn reap_all_containers(&self) {
        let _ = Command::new(BIN)
            .arg("--home")
            .arg(&self.home)
            .args(["sandbox", "reap", "--all"])
            .env("TENON_RELEASE_DIR", &self.release)
            .output();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.home.join("run/base.ready").is_file() {
            let _ = self.run(&["stop"]);
            std::thread::sleep(Duration::from_millis(500));
        }
        self.reap_all_containers();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn kept(steps: &[i64], keep_steps: i64, milestone_every: i64) -> Vec<i64> {
    let newest = steps.iter().copied().max().unwrap_or(0);
    steps
        .iter()
        .copied()
        .filter(|step| *step > newest - keep_steps || *step % milestone_every == 0)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_loop_records_episodes_tool_results_and_blobs_and_retention_bounds_them() {
    let Some(release) = skip() else { return };
    let server: Fake = fake::spawn(vec![
        Say::Tool("bash".to_string(), json!({"cmd": SPEW})),
        Say::Text("that was a lot of x".to_string()),
    ])
    .await
    .expect("fake model");
    let fixture = Fixture::new(release, &server.base_url);
    fixture.start();
    fixture.ready(Duration::from_secs(120)).await;

    // a. one turn with a tool call: two steps, two episodes, both with cost
    let (ok, out, err) = fixture.run(&["run", "spew some x", "--timeout", "120"]);
    assert!(ok, "tenon run failed: {out}{err}\n{}", fixture.log());
    let episodes = fixture
        .rpc("episodes.tail", json!({"env": "root", "n": 50}))
        .await
        .expect("episodes.tail")["episodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(episodes.len(), 2, "one episode per step: {episodes:?}");
    let session = episodes[0]["session_id"]
        .as_str()
        .expect("session")
        .to_string();
    assert_eq!(episodes[0]["step"], 1, "{}", episodes[0]);
    assert_eq!(episodes[1]["step"], 2, "{}", episodes[1]);
    assert_eq!(episodes[0]["action"][0]["name"], "bash", "{}", episodes[0]);
    assert_eq!(episodes[1]["action"], "respond", "{}", episodes[1]);
    assert_eq!(episodes[0]["verifier_score"], 1.0, "{}", episodes[0]);
    assert_eq!(episodes[0]["cost"]["total"], 18, "{}", episodes[0]);
    assert_eq!(
        episodes[0]["state_hash"].as_str().unwrap_or_default().len(),
        16,
        "{}",
        episodes[0]
    );

    // b. the 20 KB tool output is a blob the tool_results row points at
    let rows = fixture
        .rpc("tool_results.tail", json!({"env": "root", "n": 10}))
        .await
        .expect("tool_results.tail")["tool_results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["name"], "bash", "{}", rows[0]);
    assert_eq!(rows[0]["status"], "ok", "{}", rows[0]);
    assert!(rows[0]["event_id"].as_i64().unwrap_or(0) > 0, "{}", rows[0]);
    let hash = rows[0]["blob_hash"]
        .as_str()
        .unwrap_or_else(|| panic!("no blob on {}", rows[0]))
        .to_string();
    let whole = fixture
        .rpc("blobs.get", json!({"env": "root", "hash": hash}))
        .await
        .expect("blobs.get");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(whole["data"].as_str().unwrap_or_default())
        .expect("base64");
    assert!(bytes.len() > 20_000, "{} bytes", bytes.len());
    assert_eq!(whole["size"].as_i64().unwrap_or(0), bytes.len() as i64);
    let window = fixture
        .rpc(
            "blobs.get",
            json!({"env": "root", "hash": hash, "offset": 100, "len": 32}),
        )
        .await
        .expect("blobs.get window");
    let slice = base64::engine::general_purpose::STANDARD
        .decode(window["data"].as_str().unwrap_or_default())
        .expect("base64");
    assert_eq!(slice, bytes[100..132], "the incremental read is a window");
    let logged = fixture
        .events()
        .await
        .into_iter()
        .rfind(|event| event["kind"] == "tool/result")
        .expect("a tool/result event");
    assert_eq!(logged["data"]["blob"], json!(hash), "{logged}");
    assert!(
        logged["data"]["text"].as_str().unwrap_or_default().len() < bytes.len(),
        "the model saw the whole blob"
    );

    // c. replaying the events reproduces what session.history answers
    let history = fixture
        .rpc(
            "session.history",
            json!({"env": "root", "session_id": session}),
        )
        .await
        .expect("session.history")["events"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let replayed: Vec<Value> = fixture
        .events()
        .await
        .into_iter()
        .filter(|event| event["data"]["session"] == json!(session))
        .collect();
    let ids = |rows: &[Value]| -> Vec<(i64, String)> {
        rows.iter()
            .map(|row| {
                (
                    row["id"].as_i64().unwrap_or(0),
                    row["kind"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    };
    assert!(!history.is_empty(), "the session logged nothing");
    assert_eq!(ids(&replayed), ids(&history), "the fold lost rows");

    // d. a hundred recorded steps and a dozen packs, bounded by state.retain
    let mut client = fixture.client().await;
    for step in 0..100 {
        let body = format!("step {step} output {}", "y".repeat(200));
        let data = base64::engine::general_purpose::STANDARD.encode(body.as_bytes());
        let blob = client
            .call("blobs.put", json!({"env": "root", "data": data}))
            .await
            .expect("blobs.put");
        let event = client
            .call(
                "events.append",
                json!({
                    "env": "root",
                    "kind": "tool/result",
                    "data": {"session": "loop", "step": step},
                }),
            )
            .await
            .expect("events.append");
        client
            .call(
                "tool_results.append",
                json!({
                    "env": "root",
                    "event_id": event["id"],
                    "name": "bash",
                    "status": "ok",
                    "duration_ms": 3,
                    "blob_hash": blob["hash"],
                }),
            )
            .await
            .expect("tool_results.append");
        client
            .call(
                "episodes.append",
                json!({
                    "env": "root",
                    "session_id": "loop",
                    "step": step,
                    "action": "respond",
                    "verifier_score": 1.0,
                    "cost": {"total": 1},
                    "user_event": event["id"],
                }),
            )
            .await
            .expect("episodes.append");
    }
    for round in 0..12 {
        fixture
            .tool(
                "fs.write",
                json!({"path": format!("step{round}.txt"), "content": format!("{round}\n")}),
            )
            .await
            .expect("fs.write");
        fixture
            .tool("snap.commit", json!({"label": format!("step{round}")}))
            .await
            .expect("snap.commit");
        fixture
            .rpc("snap.pull", json!({"env": "root"}))
            .await
            .expect("snap.pull");
    }
    let packs = fixture
        .rpc("snap.list", json!({"env": "root"}))
        .await
        .expect("snap.list")["packs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let steps: Vec<i64> = packs
        .iter()
        .map(|pack| pack["step"].as_i64().unwrap_or(0))
        .collect();
    assert!(steps.len() >= 12, "only {} packs: {packs:?}", steps.len());
    let survivors = kept(&steps, 5, 10);
    let retained = fixture
        .rpc("state.retain", json!({"env": "root"}))
        .await
        .expect("state.retain");
    assert_eq!(
        retained["left"]["packs"].as_i64().unwrap_or(0),
        survivors.len() as i64,
        "packs left: {retained}, expected {survivors:?} of {steps:?}"
    );
    assert!(
        retained["removed"]["packs"].as_i64().unwrap_or(0) > 0,
        "{retained}"
    );
    let left = fixture
        .rpc("snap.list", json!({"env": "root"}))
        .await
        .expect("snap.list")["packs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let after: Vec<i64> = left
        .iter()
        .map(|pack| pack["step"].as_i64().unwrap_or(0))
        .collect();
    assert_eq!(after, survivors, "the wrong steps survived");
    assert_eq!(
        retained["left"]["events"].as_i64().unwrap_or(0),
        50,
        "{retained}"
    );
    assert!(
        retained["removed"]["blobs"].as_i64().unwrap_or(0) >= 40,
        "the blobs of pruned tool results stayed: {retained}"
    );
    let blobs_left = retained["left"]["blobs"].as_i64().unwrap_or(0);
    assert!(
        (1..=60).contains(&blobs_left),
        "blobs are unbounded: {retained}"
    );
    let episodes = fixture
        .rpc("episodes.tail", json!({"env": "root", "n": 500}))
        .await
        .expect("episodes.tail");
    assert_eq!(
        episodes["count"].as_i64().unwrap_or(0),
        102,
        "episodes are the navigator's data, retention does not touch them"
    );
}
