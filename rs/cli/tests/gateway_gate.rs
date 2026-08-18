use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tenon_base::client::Client;

const BIN: &str = env!("CARGO_BIN_EXE_tenon");
const NAME: &str = "gateway-gate";

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
        std::fs::write(home.join("config.yml"), "sandbox: oci\n").unwrap();
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

    fn sock(&self) -> PathBuf {
        self.home.join("run/base.sock")
    }

    fn workspace(&self) -> PathBuf {
        self.home.join("envs/root/workspace")
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut client = Client::connect(&self.sock())
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
}

impl Fixture {
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

fn root_tree(status: &Value) -> Value {
    status["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["env"] == "root")
        .cloned()
        .unwrap_or(Value::Null)["tree"]
        .clone()
}

fn find(tree: &Value, id: &str) -> Option<Value> {
    if tree["id"] == id {
        return Some(tree.clone());
    }
    tree["children"]
        .as_array()?
        .iter()
        .find_map(|child| find(child, id))
}

fn gateway_children(status: &Value) -> Vec<Value> {
    let tree = root_tree(status);
    match find(&tree, "gateway") {
        Some(gateway) => gateway["children"].as_array().cloned().unwrap_or_default(),
        None => vec![],
    }
}

fn active(children: &[Value]) -> Vec<String> {
    children
        .iter()
        .filter(|child| child["status"] != "failed")
        .filter_map(|child| child["id"].as_str().map(str::to_string))
        .collect()
}

const PLUGIN_BODY: &str = r#"
import sys
sys.path.insert(0, "/workspace")
import tenon

plugin = tenon.Plugin(inject=[])
plugin.provide("inside", {"ping": lambda: "pong"})
plugin.run()
"#;

#[tokio::test]
async fn a_plugin_started_inside_the_sandbox_registers_through_the_gateway() {
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
    fixture.start();

    let status = fixture.status().await;
    let root = status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["env"] == "root")
        .cloned()
        .unwrap();
    assert_eq!(root["sandbox"]["backend"], "oci", "{status}");
    let before = active(&gateway_children(&status));

    let workspace = fixture.workspace();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !workspace.is_dir() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(workspace.is_dir(), "no workspace dir at {workspace:?}");

    let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/py/tenon.py");
    std::fs::copy(&sdk, workspace.join("tenon.py")).expect("copy sdk/py/tenon.py");
    std::fs::write(workspace.join("inside_plugin.py"), PLUGIN_BODY).expect("write plugin");

    let launch = fixture
        .rpc(
            "sandbox.exec",
            json!({
                "env": "root",
                "cmd": "sh",
                "args": [
                    "-c",
                    "nohup python3 /workspace/inside_plugin.py \
                     >/workspace/inside.log 2>&1 </dev/null & echo started"
                ],
                "timeout": 10_000,
            }),
        )
        .await
        .expect("sandbox.exec launch");
    assert_eq!(launch["status"], 0, "{launch}\n{}", fixture.log());

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut after = before.clone();
    while Instant::now() < deadline {
        let status = fixture.status().await;
        after = active(&gateway_children(&status));
        if after.len() > before.len() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        after.len() > before.len(),
        "no new gateway fiber appeared: before={before:?} after={after:?}\n{}",
        fixture.log()
    );

    let ping = fixture
        .rpc(
            "svc",
            json!({"env": "root", "name": "inside", "method": "ping", "args": []}),
        )
        .await
        .expect("svc ping");
    assert_eq!(ping, "pong", "{ping}");

    let destroyed = fixture
        .rpc("sandbox.destroy", json!({"env": "root"}))
        .await
        .expect("sandbox.destroy");
    assert_eq!(destroyed["ok"], true);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut gone = false;
    while Instant::now() < deadline {
        let status = fixture.status().await;
        if active(&gateway_children(&status)).len() < after.len() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(gone, "the plugin fiber outlived its sandbox instance");

    let status = fixture.status().await;
    assert_eq!(status["nodes"].as_array().unwrap().len(), 2, "{status}");

    let (ok, text) = fixture.run(&["reset"]);
    assert!(ok, "reset failed: {text}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut registered = false;
    while Instant::now() < deadline {
        let status = fixture.status().await;
        let root = status["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["env"] == "root")
            .cloned()
            .unwrap();
        if root["registered"] == true {
            registered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(registered, "root did not come back after the reset");
    let status = fixture.status().await;
    assert_eq!(status["nodes"].as_array().unwrap().len(), 2);
}
