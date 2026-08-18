use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tenon_base::client::Client;

const BIN: &str = env!("CARGO_BIN_EXE_tenon");
const NAME: &str = "worker-boot";

fn release() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TENON_RELEASE_DIR") {
        let dir = PathBuf::from(dir);
        return dir.join("bin/tenon_beam").is_file().then_some(dir);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = repo.join("beam/_build/prod/rel/tenon_beam");
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

struct Fixture {
    home: PathBuf,
    release: PathBuf,
}

impl Fixture {
    fn new(release: PathBuf, config: &str) -> Self {
        let home = std::env::temp_dir().join(format!("tenon-it-{}-{NAME}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.yml"), config).unwrap();
        Self { home, release }
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(BIN)
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .env("TENON_RELEASE_DIR", &self.release)
            .output()
            .expect("run tenon");
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    fn start(&self) {
        let (ok, text) = self.run(&["start"]);
        assert!(ok, "start failed: {text}\n{}", self.log());
    }

    fn log(&self) -> String {
        ["base", "guardian", "root"]
            .iter()
            .map(|name| {
                let path = self.home.join(format!("run/{name}.log"));
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                format!("--- {name}.log\n{body}")
            })
            .collect()
    }

    fn workspace(&self) -> PathBuf {
        self.home.join("envs/root/workspace")
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut client = Client::connect(&self.home.join("run/base.sock"))
            .await
            .map_err(|error| error.to_string())?;
        client
            .call(method, params)
            .await
            .map_err(|error| error.to_string())
    }

    async fn status(&self) -> Value {
        self.rpc("status", json!({})).await.expect("status")
    }

    async fn node(&self, env: &str) -> Value {
        self.status().await["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|node| node["env"] == env)
            .cloned()
            .unwrap_or(Value::Null)
    }

    async fn worker_ready(&self, env: &str, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        let mut last = Value::Null;
        while Instant::now() < deadline {
            last = self.node(env).await;
            if last["worker"]["state"] == "ready" {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        panic!(
            "worker never became ready for {env}: {last}\n{}",
            self.log()
        );
    }

    async fn tool(&self, env: &str, method: &str, params: Value) -> Result<Value, String> {
        self.rpc(
            "svc",
            json!({"env": env, "name": "worker", "method": method, "args": [params]}),
        )
        .await
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

#[tokio::test]
async fn the_env_boots_a_worker_pulls_its_packs_and_restores_them_on_reset() {
    let Some(release) = skip() else { return };
    let fixture = Fixture::new(release, "sandbox: oci\nworker:\n  pull_interval_ms: 2000\n");
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

    let (ok, text) = fixture.run(&["reset"]);
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
