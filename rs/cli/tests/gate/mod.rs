#![allow(dead_code)]

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tenon_base::client::Client;

pub const BIN: &str = env!("CARGO_BIN_EXE_tenon");

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn release() -> Option<PathBuf> {
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

/// The same skip rule every P3 gate uses: without a release or a container
/// engine the test prints why and passes.
pub fn skip(name: &str) -> Option<PathBuf> {
    if !oci_available() {
        println!("skipping {name}: neither podman nor docker found in PATH");
        return None;
    }
    match release() {
        Some(dir) => Some(dir),
        None => {
            println!(
                "skipping {name}: no beam release. Build it with \
                 `cd beam && MIX_ENV=prod mix release` or set TENON_RELEASE_DIR"
            );
            None
        }
    }
}

pub struct Fixture {
    pub home: PathBuf,
    pub release: PathBuf,
}

impl Fixture {
    pub fn new(name: &str, release: PathBuf, config: &str, harness: &str) -> Self {
        let home = std::env::temp_dir().join(format!("tenon-it-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("profiles/root")).unwrap();
        std::fs::write(home.join("config.yml"), config).unwrap();
        std::fs::write(home.join("profiles/root/harness.yml"), harness).unwrap();
        Self { home, release }
    }

    pub fn run(&self, args: &[&str]) -> (bool, String, String) {
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

    /// A `tenon` invocation that is expected to block (a `run` waiting behind
    /// an approval), collected later with `wait`.
    pub fn spawn(&self, args: &[&str]) -> Child {
        Command::new(BIN)
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .env("TENON_RELEASE_DIR", &self.release)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tenon")
    }

    pub fn start(&self) {
        let (ok, out, err) = self.run(&["start"]);
        assert!(ok, "start failed: {out}{err}\n{}", self.log());
    }

    pub fn log(&self) -> String {
        ["base", "guardian", "root", "harness-root"]
            .iter()
            .map(|name| {
                let path = self.home.join(format!("run/{name}.log"));
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                format!("--- {name}.log\n{body}")
            })
            .collect()
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let mut client = Client::connect(&self.home.join("run/base.sock"))
            .await
            .map_err(|error| error.to_string())?;
        client
            .call(method, params)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn node(&self, env: &str) -> Value {
        self.rpc("status", json!({})).await.expect("status")["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|node| node["env"] == env)
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub async fn ready(&self, limit: Duration) -> Value {
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

    pub async fn events(&self) -> Vec<Value> {
        self.rpc("events.tail", json!({"env": "root", "limit": 5000}))
            .await
            .expect("events.tail")["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub async fn of_kind(&self, kind: &str) -> Vec<Value> {
        self.events()
            .await
            .into_iter()
            .filter(|event| event["kind"] == kind)
            .map(|event| event["data"].clone())
            .collect()
    }

    /// Polls `tenon approvals` until a pending row's reason matches, and
    /// answers with its id.
    pub async fn await_approval(&self, needle: &str, limit: Duration) -> i64 {
        let deadline = Instant::now() + limit;
        let mut last = String::new();
        while Instant::now() < deadline {
            let (ok, out, _err) = self.run(&["approvals"]);
            last = out.clone();
            if ok {
                if let Some(line) = out.lines().find(|line| line.contains(needle)) {
                    if let Some(id) = line.split_whitespace().next() {
                        if let Ok(id) = id.parse::<i64>() {
                            return id;
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        panic!(
            "no pending approval matching {needle:?}: {last}\n{}",
            self.log()
        );
    }

    pub async fn await_status(
        &self,
        limit: Duration,
        mut pred: impl FnMut(&Value) -> bool,
    ) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Ok(status) = self.rpc("status", json!({})).await {
                if pred(&status) {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        false
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
        let _ = std::fs::remove_file(self.home.join("run/STOP"));
        if self.home.join("run/base.ready").is_file() {
            let _ = self.run(&["stop"]);
            std::thread::sleep(Duration::from_millis(500));
        }
        self.reap_all_containers();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

pub fn collect(child: Child) -> (bool, String, String) {
    let output = child.wait_with_output().expect("collect output");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}
