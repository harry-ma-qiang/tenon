use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tenon_base::client::Client;

const BIN: &str = env!("CARGO_BIN_EXE_tenon");
const NAME: &str = "spawn-gate";
/// The limits under test are the environment tree's, not the human gate's, so
/// `spawn_soft_limit: 0` turns the P3.5 approval gate off for this home.
const CONFIG: &str = "sandbox: oci\nenvs:\n  max_total: 3\n  max_depth: 1\n  ram_mb: 384\n\
                      approval:\n  spawn_soft_limit: 0\n";

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
    fn new(release: PathBuf) -> Self {
        let home = std::env::temp_dir().join(format!("tenon-it-{}-{NAME}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.yml"), CONFIG).unwrap();
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

    fn log(&self) -> String {
        ["base", "guardian", "root", "root.1"]
            .iter()
            .map(|name| {
                let path = self.home.join(format!("run/{name}.log"));
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                format!("--- {name}.log\n{body}")
            })
            .collect()
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

    async fn worker_ready(&self, env: &str, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut last = Value::Null;
        while Instant::now() < deadline {
            last = self.node(env).await;
            if last["worker"]["state"] == "ready" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        panic!(
            "worker never became ready for {env}: {last}\n{}",
            self.log()
        );
    }

    async fn registered(&self, env: &str, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        let mut last = Value::Null;
        while Instant::now() < deadline {
            last = self.node(env).await;
            if last["registered"] == true {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        panic!("{env} never registered: {last}\n{}", self.log());
    }

    async fn exec(&self, env: &str, line: &str) -> Value {
        self.rpc(
            "sandbox.exec",
            json!({"env": env, "cmd": "sh", "args": ["-c", line], "timeout": 15_000}),
        )
        .await
        .expect("sandbox.exec")
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

fn gateway_children(node: &Value) -> usize {
    fn find(tree: &Value, id: &str) -> Option<Value> {
        if tree["id"] == id {
            return Some(tree.clone());
        }
        tree["children"]
            .as_array()?
            .iter()
            .find_map(|child| find(child, id))
    }
    match find(&node["tree"], "gateway") {
        Some(gateway) => gateway["children"].as_array().map(Vec::len).unwrap_or(0),
        None => 0,
    }
}

#[tokio::test]
async fn a_child_env_is_a_fiber_of_its_parent_and_dies_with_it() {
    if !oci_available() {
        println!("skipping {NAME}: neither podman nor docker found in PATH");
        return;
    }
    let Some(release) = release() else {
        println!(
            "skipping {NAME}: no beam release. Build it with \
             `cd beam && MIX_ENV=prod mix release` or set TENON_RELEASE_DIR"
        );
        return;
    };
    let fixture = Fixture::new(release);
    let (ok, text) = fixture.run(&["start"]);
    assert!(ok, "start failed: {text}");
    fixture.worker_ready("root", Duration::from_secs(90)).await;
    let before = gateway_children(&fixture.node("root").await);

    let child = fixture
        .rpc("runtime.spawn", json!({"parent": "root", "overrides": {}}))
        .await
        .expect("runtime.spawn");
    assert_eq!(child["env"], "root.1", "{child}");
    assert_eq!(child["depth"], 1, "{child}");
    assert_eq!(child["ram_mb"], 384, "{child}");
    assert!(
        child["profile"]
            .as_str()
            .unwrap_or_default()
            .contains("overlay.patch.yml"),
        "the child got no patch layer: {child}"
    );

    fixture.registered("root.1", Duration::from_secs(60)).await;
    let root = fixture.node("root").await;
    assert_eq!(root["children"], json!(["root.1"]), "{root}");
    let spawned = fixture.node("root.1").await;
    assert_eq!(spawned["parent"], "root", "{spawned}");
    assert_eq!(spawned["depth"], 1, "{spawned}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut after = before;
    while Instant::now() < deadline {
        after = gateway_children(&fixture.node("root").await);
        if after > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    assert!(
        after > before,
        "the child never appeared as a fiber in its parent's tree ({before} -> {after})"
    );

    let deep = fixture
        .rpc(
            "runtime.spawn",
            json!({"parent": "root.1", "overrides": {}}),
        )
        .await
        .expect_err("depth 2 must be refused");
    assert!(deep.contains("depth"), "{deep}");

    fixture
        .rpc("runtime.spawn", json!({"parent": "root", "overrides": {}}))
        .await
        .expect("the second child is still inside the limit");
    let over = fixture
        .rpc("runtime.spawn", json!({"parent": "root", "overrides": {}}))
        .await
        .expect_err("a fourth environment must be refused");
    assert!(over.contains("limit"), "{over}");

    let parent_sock = fixture.home.join("run/gw-root/gateway.sock");
    let base_sock = fixture.home.join("run/base.sock");
    let seen = fixture
        .exec(
            "root.1",
            &format!(
                "test -e {} && echo parent-gateway-visible; test -e {} && echo base-sock-visible; \
                 python3 -c \"import socket;socket.socket(socket.AF_UNIX).connect('{}')\" \
                 2>/dev/null && echo connected; echo checked",
                parent_sock.display(),
                base_sock.display(),
                parent_sock.display(),
            ),
        )
        .await;
    let text = seen["stdout"].as_str().unwrap_or_default();
    assert!(text.contains("checked"), "{seen}");
    assert!(
        !text.contains("visible") && !text.contains("connected"),
        "a child reached its parent's sockets: {seen}"
    );

    let pid = fixture.node("root").await["pid"]
        .as_i64()
        .expect("root pid");
    let _ = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill the parent node");
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut gone = false;
    while Instant::now() < deadline {
        if fixture.node("root.1").await.is_null() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    assert!(gone, "the child outlived its parent\n{}", fixture.log());
    fixture.registered("root", Duration::from_secs(60)).await;
}
