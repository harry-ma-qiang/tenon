use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

static LOCK: Mutex<()> = Mutex::new(());
const BIN: &str = env!("CARGO_BIN_EXE_tenon");

struct Fixture {
    home: PathBuf,
    release: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

fn release() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TENON_RELEASE_DIR") {
        let dir = PathBuf::from(dir);
        return dir.join("bin/tenon_beam").is_file().then_some(dir);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = repo.join("beam/_build/prod/rel/tenon_beam");
    dir.join("bin/tenon_beam").is_file().then_some(dir)
}

fn fixture(name: &str) -> Option<Fixture> {
    let guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let Some(release) = release() else {
        println!(
            "skipping {name}: no beam release. Build it with \
             `cd beam && MIX_ENV=prod mix release` or set TENON_RELEASE_DIR"
        );
        return None;
    };
    let home = std::env::temp_dir().join(format!("tenon-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    Some(Fixture {
        home,
        release,
        _guard: guard,
    })
}

impl Fixture {
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

    fn start(&self, extra: &[&str]) {
        let mut args = vec!["start"];
        args.extend_from_slice(extra);
        let (ok, text) = self.run(&args);
        assert!(ok, "start failed: {text}\n{}", self.log());
    }

    fn status(&self) -> Value {
        let (ok, text) = self.run(&["status"]);
        assert!(ok, "status failed: {text}");
        serde_json::from_str(&text).expect("status json")
    }

    fn node(&self, env: &str) -> Value {
        let status = self.status();
        status["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|node| node["env"] == env)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn base_pid(&self) -> i64 {
        let text = std::fs::read_to_string(self.home.join("run/base.ready")).expect("ready file");
        text.trim().parse().expect("base pid")
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

    fn await_fresh(&self, env: &str, old: i64) -> i64 {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let node = self.node(env);
            let pid = node["pid"].as_i64().unwrap_or(0);
            if node["registered"] == true && pid != old {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("{env} never came back after {old}\n{}", self.log());
    }

    /// `killing_base_takes_every_node_down` leaks its sandbox container by
    /// design (nothing runs `destroy` across a killed base, and this home
    /// never boots again); sweep it with the human `--all` reap on teardown.
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

fn alive(pid: i64) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(tail) = stat.rsplit(')').next() else {
        return false;
    };
    !matches!(tail.split_whitespace().next(), Some("Z") | None)
}

fn wait_gone(pids: &[i64], limit: Duration) -> Duration {
    let started = Instant::now();
    while started.elapsed() < limit {
        if pids.iter().all(|pid| !alive(*pid)) {
            return started.elapsed();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    started.elapsed()
}

fn kill(pid: i64, signal: &str) {
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .expect("kill");
    assert!(status.success(), "kill {signal} {pid}");
}

#[test]
fn the_harness_without_a_base_socket_fails_loudly() {
    let output = Command::new(BIN)
        .arg("harness")
        .env_remove("TENON_BASE_SOCK")
        .output()
        .expect("run tenon");
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("TENON_BASE_SOCK"), "{text}");
}

#[test]
fn the_worker_without_a_reachable_gateway_fails_loudly() {
    let dir = std::env::temp_dir().join(format!("tenon-it-{}-nowire", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let output = Command::new(BIN)
        .arg("worker")
        .arg("--workspace")
        .arg(&dir)
        .env(
            "TENON_GATEWAY",
            format!("unix:{}/absent.sock", dir.display()),
        )
        .output()
        .expect("run tenon worker");
    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(text.contains("connect"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_base_is_an_error_not_a_hang() {
    let home = std::env::temp_dir().join(format!("tenon-it-{}-nobase", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let output = Command::new(BIN)
        .arg("--home")
        .arg(&home)
        .arg("status")
        .output()
        .expect("run tenon");
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("is the base running?"), "{text}");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn boot_registers_both_nodes_and_mounts_the_demo_plugin() {
    let Some(fixture) = fixture("boot") else {
        return;
    };
    fixture.start(&[]);
    let status = fixture.status();
    let envs: Vec<&str> = status["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["env"].as_str().unwrap())
        .collect();
    assert_eq!(envs, vec!["guardian", "root"]);

    let guardian = fixture.node("guardian");
    assert_eq!(guardian["role"], "guardian");
    assert_eq!(guardian["registered"], true);
    let ids = fiber_ids(&guardian["tree"]);
    assert!(ids.contains(&"guardian".to_string()), "{ids:?}");
    assert!(ids.contains(&"link".to_string()), "{ids:?}");

    let root = fixture.node("root");
    assert_eq!(root["role"], "agent");
    assert_eq!(root["registered"], true);
    assert!(
        root["sandbox"]["backend"] == "oci" || root["sandbox"]["backend"] == "landlock",
        "unexpected sandbox on the default auto profile: {}",
        root["sandbox"]
    );
    let ids = fiber_ids(&root["tree"]);
    assert!(
        ids.contains(&"demo".to_string()),
        "no demo plugin in {ids:?}"
    );
    assert!(fixture.home.join("lkg/profiles/root/tenon.yml").is_file());
}

#[test]
fn reset_replaces_the_env_and_leaves_the_guardian_alone() {
    let Some(fixture) = fixture("reset") else {
        return;
    };
    fixture.start(&[]);
    let before = fixture.node("root")["pid"].as_i64().unwrap();
    let guardian = fixture.node("guardian")["pid"].as_i64().unwrap();

    let (ok, text) = fixture.run(&["reset"]);
    assert!(ok, "reset failed: {text}");
    let after = fixture.await_fresh("root", before);
    assert_ne!(before, after, "reset kept the same pid");
    assert!(!alive(before), "the old env is still running");
    assert_eq!(fixture.node("guardian")["pid"].as_i64().unwrap(), guardian);
    assert!(alive(guardian), "the guardian went down with the env");
}

#[test]
fn killing_base_takes_every_node_down() {
    let Some(fixture) = fixture("kill") else {
        return;
    };
    fixture.start(&[]);
    let base = fixture.base_pid();
    let nodes = vec![
        fixture.node("guardian")["pid"].as_i64().unwrap(),
        fixture.node("root")["pid"].as_i64().unwrap(),
    ];
    kill(base, "-9");
    let took = wait_gone(&nodes, Duration::from_secs(5));
    for pid in &nodes {
        assert!(!alive(*pid), "node {pid} survived base after {took:?}");
    }
}

#[test]
fn stop_shuts_down_base_and_both_nodes() {
    let Some(fixture) = fixture("stop") else {
        return;
    };
    fixture.start(&[]);
    let base = fixture.base_pid();
    let mut pids = vec![base];
    pids.push(fixture.node("guardian")["pid"].as_i64().unwrap());
    pids.push(fixture.node("root")["pid"].as_i64().unwrap());
    let (ok, text) = fixture.run(&["stop"]);
    assert!(ok, "stop failed: {text}");
    let took = wait_gone(&pids, Duration::from_secs(15));
    for pid in &pids {
        assert!(!alive(*pid), "{pid} survived stop after {took:?}");
    }
    assert!(!fixture.home.join("run/base.sock").exists());
    assert!(!fixture.home.join("run/base.ready").exists());
}

#[test]
fn an_env_that_dies_is_restarted_by_base() {
    let Some(fixture) = fixture("restart") else {
        return;
    };
    fixture.start(&[]);
    let before = fixture.node("root")["pid"].as_i64().unwrap();
    kill(before, "-9");
    let after = fixture.await_fresh("root", before);
    assert_ne!(before, after);
    assert_eq!(fixture.node("root")["restarts"], 1);
}

fn fiber_ids(tree: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(id) = tree["id"].as_str() {
        ids.push(id.to_string());
    }
    if let Some(children) = tree["children"].as_array() {
        for child in children {
            ids.extend(fiber_ids(child));
        }
    }
    ids
}
