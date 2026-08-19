use crate::procs::{alive, kill, pids_by_home, pids_by_sock};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tenon_base::client::Client;

/// Every fixture that boots a base takes this before it starts, so two homes
/// never race over the container engine or the machine's memory.
static LOCK: Mutex<()> = Mutex::new(());

const LOGS: [&str; 5] = ["base", "guardian", "root", "root.1", "harness-root"];

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

/// The release-only skip rule: a gate that needs no container engine.
pub fn skip_release(name: &str) -> Option<PathBuf> {
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

/// The same skip rule every P3 gate uses: without a release or a container
/// engine the test prints why and passes.
pub fn skip(name: &str) -> Option<PathBuf> {
    if !oci_available() {
        println!("skipping {name}: neither podman nor docker found in PATH");
        return None;
    }
    skip_release(name)
}

#[derive(Default)]
pub struct Spec<'a> {
    pub name: &'a str,
    pub config: Option<&'a str>,
    pub harness: Option<&'a str>,
    /// Serialize this fixture against every other locked one.
    pub lock: bool,
    /// Kill whatever base or node processes the home still owns on teardown:
    /// what a suite that kills base with -9 needs and a happy-path gate does
    /// not.
    pub reap_pids: bool,
    /// A cap on every `run_text` of this fixture. A suite that runs against a
    /// base it is busy breaking wants one; a gate whose `start` legitimately
    /// takes minutes does not.
    pub limit: Option<Duration>,
}

pub struct Fixture {
    pub home: PathBuf,
    pub release: PathBuf,
    pub bin: String,
    reap_pids: bool,
    limit: Option<Duration>,

    _guard: Option<MutexGuard<'static, ()>>,
}

impl Fixture {
    pub fn open(bin: &str, release: PathBuf, spec: Spec) -> Self {
        let guard = spec
            .lock
            .then(|| LOCK.lock().unwrap_or_else(|error| error.into_inner()));
        let home =
            std::env::temp_dir().join(format!("tenon-it-{}-{}", std::process::id(), spec.name));
        let _ = std::fs::remove_dir_all(&home);
        match spec.harness {
            Some(_) => std::fs::create_dir_all(home.join("profiles/root")).expect("home"),
            None => std::fs::create_dir_all(&home).expect("home"),
        }
        if let Some(config) = spec.config {
            std::fs::write(home.join("config.yml"), config).expect("write config.yml");
        }
        if let Some(harness) = spec.harness {
            std::fs::write(home.join("profiles/root/harness.yml"), harness).expect("write harness");
        }
        Self {
            home,
            release,
            bin: bin.to_string(),
            reap_pids: spec.reap_pids,
            limit: spec.limit,
            _guard: guard,
        }
    }

    pub fn new(bin: &str, name: &str, release: PathBuf, config: &str, harness: &str) -> Self {
        Self::open(
            bin,
            release,
            Spec {
                name,
                config: Some(config),
                harness: Some(harness),
                ..Spec::default()
            },
        )
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .env("TENON_RELEASE_DIR", &self.release);
        command
    }

    pub fn run(&self, args: &[&str]) -> (bool, String, String) {
        let output = self.command(args).output().expect("run tenon");
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// stdout and stderr as one body: what a test that only greps the output
    /// wants.
    pub fn run_text(&self, args: &[&str]) -> (bool, String) {
        if let Some(limit) = self.limit {
            return self.run_timeout(args, limit);
        }
        let (ok, out, err) = self.run(args);
        (ok, format!("{out}{err}"))
    }

    pub fn run_timeout(&self, args: &[&str], limit: Duration) -> (bool, String) {
        let mut child = self.spawn(args);
        let deadline = Instant::now() + limit;
        loop {
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return (false, format!("timed out after {limit:?}"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let (ok, out, err) = collect(child);
        (ok, format!("{out}{err}"))
    }

    /// A `tenon` invocation that is expected to block, collected later.
    pub fn spawn(&self, args: &[&str]) -> Child {
        self.command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tenon")
    }

    pub fn start(&self) {
        self.start_with(&[]);
    }

    pub fn start_with(&self, extra: &[&str]) {
        let mut args = vec!["start"];
        args.extend_from_slice(extra);
        let (ok, text) = self.run_text(&args);
        assert!(ok, "start failed: {text}\n{}", self.log());
    }

    pub fn log(&self) -> String {
        LOGS.iter()
            .map(|name| {
                let path = self.home.join(format!("run/{name}.log"));
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                format!("--- {name}.log\n{body}")
            })
            .collect()
    }

    pub fn sock(&self) -> PathBuf {
        self.home.join("run/base.sock")
    }

    pub fn workspace(&self) -> PathBuf {
        self.home.join("envs/root/workspace")
    }

    pub fn cli_status(&self) -> Value {
        let (ok, text) = self.run_text(&["status"]);
        assert!(ok, "status failed: {text}");
        serde_json::from_str(&text).expect("status json")
    }

    pub fn cli_status_result(&self) -> Result<Value, String> {
        let (ok, text) = self.run_timeout(&["status"], Duration::from_secs(20));
        match ok {
            true => serde_json::from_str(&text).map_err(|error| error.to_string()),
            false => Err(text),
        }
    }

    pub fn cli_node(&self, env: &str) -> Value {
        node_of(&self.cli_status(), env)
    }

    pub fn base_pid(&self) -> i64 {
        let path = self.home.join("run/base.ready");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let pid = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| text.trim().parse().ok());
            if let Some(pid) = pid {
                return pid;
            }
            if Instant::now() >= deadline {
                panic!("no valid pid in {}", path.display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn await_fresh(&self, env: &str, old: i64) -> i64 {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let node = self.cli_node(env);
            let pid = node["pid"].as_i64().unwrap_or(0);
            if node["registered"] == true && pid != old && pid != 0 {
                return pid;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("{env} never came back after {old}\n{}", self.log());
    }

    pub fn await_condition(&self, limit: Duration, mut pred: impl FnMut(&Value) -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Ok(status) = self.cli_status_result() {
                if pred(&status) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        false
    }

    /// The guardian and agent BEAM node processes of this home.
    pub fn node_pids(&self) -> Vec<i64> {
        pids_by_sock(&self.sock())
    }

    /// The base process(es) of this home: more than one at a time means a
    /// double boot.
    pub fn base_pids(&self) -> Vec<i64> {
        pids_by_home(&self.home, &self.bin)
    }

    pub fn all_pids(&self) -> Vec<i64> {
        let mut pids = self.node_pids();
        pids.extend(self.base_pids());
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    /// A scenario that killed base with -9 leaks that boot's sandbox container
    /// by design; teardown sweeps it with the human `--all` reap rather than
    /// leaving it for the box's `podman ps -a` to accumulate.
    pub fn reap_all_containers(&self) {
        let _ = self.command(&["sandbox", "reap", "--all"]).output();
    }

    pub async fn client(&self) -> Client {
        Client::connect(&self.sock())
            .await
            .expect("connect to base")
    }

    pub async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        Client::connect(&self.sock())
            .await
            .map_err(|error| error.to_string())?
            .call(method, params)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn status(&self) -> Value {
        self.rpc("status", json!({})).await.expect("status")
    }

    pub async fn node(&self, env: &str) -> Value {
        node_of(&self.status().await, env)
    }

    /// Waits for `worker.state: ready` and `harness.state: ready`, which is
    /// what "the env can run a turn" means.
    pub async fn ready(&self, limit: Duration) -> Value {
        self.until(limit, "root never became ready", |node| {
            node["worker"]["state"] == "ready" && node["harness"]["state"] == "ready"
        })
        .await
    }

    pub async fn worker_ready(&self, env: &str, limit: Duration) -> Value {
        self.until_env(env, limit, "worker never became ready", |node| {
            node["worker"]["state"] == "ready"
        })
        .await
    }

    pub async fn registered(&self, env: &str, limit: Duration) -> Value {
        self.until_env(env, limit, "never registered", |node| {
            node["registered"] == true
        })
        .await
    }

    async fn until(&self, limit: Duration, why: &str, done: impl Fn(&Value) -> bool) -> Value {
        self.until_env("root", limit, why, done).await
    }

    async fn until_env(
        &self,
        env: &str,
        limit: Duration,
        why: &str,
        done: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + limit;
        let mut last = Value::Null;
        while Instant::now() < deadline {
            last = self.node(env).await;
            if done(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        panic!("{why} for {env}: {last}\n{}", self.log());
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

    pub async fn events_of(&self, env: &str) -> Vec<Value> {
        self.rpc("events.tail", json!({"env": env, "limit": 5000}))
            .await
            .expect("events.tail")["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub async fn events(&self) -> Vec<Value> {
        self.events_of("root").await
    }

    /// The `data` of every root event of one kind.
    pub async fn of_kind(&self, kind: &str) -> Vec<Value> {
        self.events()
            .await
            .into_iter()
            .filter(|event| event["kind"] == kind)
            .map(|event| event["data"].clone())
            .collect()
    }

    /// Boot, restore, privilege and probe facts are base-wide, so they are in
    /// the barebone's own log rather than an env's.
    pub async fn base_events(&self, kind: &str) -> Vec<Value> {
        self.events_of("base")
            .await
            .into_iter()
            .filter(|event| event["kind"] == kind)
            .collect()
    }

    pub async fn tool(&self, env: &str, method: &str, params: Value) -> Result<Value, String> {
        self.rpc(
            "svc",
            json!({"env": env, "name": "worker", "method": method, "args": [params]}),
        )
        .await
    }

    pub async fn exec(&self, env: &str, line: &str) -> Value {
        self.rpc(
            "sandbox.exec",
            json!({"env": env, "cmd": "sh", "args": ["-c", line], "timeout": 15_000}),
        )
        .await
        .expect("sandbox.exec")
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
                let id = out
                    .lines()
                    .find(|line| line.contains(needle))
                    .and_then(|line| line.split_whitespace().next())
                    .and_then(|id| id.parse::<i64>().ok());
                if let Some(id) = id {
                    return id;
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        panic!(
            "no pending approval matching {needle:?}: {last}\n{}",
            self.log()
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.home.join("run/STOP"));
        if self.home.join("run/base.ready").is_file() {
            let _ = self.run_timeout(&["stop"], Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(500));
        }
        if self.reap_pids {
            for pid in self.all_pids() {
                if alive(pid) {
                    kill(pid, "-9");
                }
            }
        }
        self.reap_all_containers();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn node_of(status: &Value, env: &str) -> Value {
    status["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|node| node["env"] == env)
        .cloned()
        .unwrap_or(Value::Null)
}

pub fn collect(child: Child) -> (bool, String, String) {
    let output = child.wait_with_output().expect("collect output");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Polls `done` until it answers true or the limit runs out.
pub async fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    done()
}
