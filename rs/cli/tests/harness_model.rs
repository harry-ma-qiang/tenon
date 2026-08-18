use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tenon_base::client::Client;

const BIN: &str = env!("CARGO_BIN_EXE_tenon");
const NAME: &str = "harness-model";
const KEY: &str = "DEEPSEEK_API_KEY";
const BASE_URL: &str = "https://api.deepseek.com";
const MODEL: &str = "deepseek-v4-flash";

fn release() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TENON_RELEASE_DIR") {
        let dir = PathBuf::from(dir);
        return dir.join("bin/tenon_beam").is_file().then_some(dir);
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../beam/_build/prod/rel/tenon_beam");
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
        std::fs::create_dir_all(home.join("profiles/root")).unwrap();
        std::fs::write(home.join("config.yml"), "sandbox: oci\n").unwrap();
        std::fs::write(
            home.join("profiles/root/harness.yml"),
            format!(
                "llm:\n  provider: deepseek\n  base_url: {BASE_URL}\n  model: {MODEL}\n  \
                 api_key_env: {KEY}\nmax_steps: 4\n"
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

    async fn ready(&self, limit: Duration) -> Value {
        let deadline = Instant::now() + limit;
        let mut last = Value::Null;
        while Instant::now() < deadline {
            let mut client = Client::connect(&self.home.join("run/base.sock"))
                .await
                .expect("connect");
            last = client.call("status", json!({})).await.expect("status")["nodes"]
                .as_array()
                .expect("nodes")
                .iter()
                .find(|node| node["env"] == "root")
                .cloned()
                .unwrap_or(Value::Null);
            if last["harness"]["state"] == "ready" && last["worker"]["state"] == "ready" {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("root never became ready: {last}");
    }

    fn reap(&self) {
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
        self.reap();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// The real-model smoke: one `tenon run` against DeepSeek. Skipped, not
/// failed, wherever the key, a container runtime or the release is missing —
/// the key never enters the sandbox and is never printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_model_answers_one_turn() {
    if std::env::var(KEY)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipping {NAME}: {KEY} is not set");
        return;
    }
    if !oci_available() {
        println!("skipping {NAME}: neither podman nor docker found in PATH");
        return;
    }
    let Some(release) = release() else {
        println!("skipping {NAME}: no beam release, set TENON_RELEASE_DIR");
        return;
    };
    let fixture = Fixture::new(release);
    let (ok, out, err) = fixture.run(&["start"]);
    assert!(ok, "start failed: {out}{err}");
    fixture.ready(Duration::from_secs(120)).await;
    let (ok, out, err) =
        fixture.run(&["run", "reply with the single word pong", "--timeout", "180"]);
    assert!(ok, "tenon run failed: {out}{err}");
    assert!(
        out.to_lowercase().contains("pong"),
        "the model did not answer pong: {out:?}"
    );
    assert!(err.contains("usage"), "no usage reported: {err:?}");
}
