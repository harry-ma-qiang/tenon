use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub static LOCK: Mutex<()> = Mutex::new(());
pub const BIN: &str = env!("CARGO_BIN_EXE_tenon");

pub struct Fixture {
    pub home: PathBuf,
    pub release: PathBuf,
    _guard: MutexGuard<'static, ()>,
}

pub fn release() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TENON_RELEASE_DIR") {
        let dir = PathBuf::from(dir);
        return dir.join("bin/tenon_beam").is_file().then_some(dir);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let dir = repo.join("beam/_build/prod/rel/tenon_beam");
    dir.join("bin/tenon_beam").is_file().then_some(dir)
}

pub fn fixture(name: &str) -> Option<Fixture> {
    fixture_with_config(name, None)
}

pub fn fixture_with_config(name: &str, config_yaml: Option<&str>) -> Option<Fixture> {
    let guard = LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let Some(release) = release() else {
        println!(
            "skipping {name}: no beam release. Build it with \
             `cd beam && MIX_ENV=prod mix release` or set TENON_RELEASE_DIR"
        );
        return None;
    };
    let home = std::env::temp_dir().join(format!("tenon-adv-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    if let Some(yaml) = config_yaml {
        std::fs::write(home.join("config.yml"), yaml).expect("write config.yml");
    }
    Some(Fixture {
        home,
        release,
        _guard: guard,
    })
}

impl Fixture {
    pub fn run(&self, args: &[&str]) -> (bool, String) {
        self.run_timeout(args, Duration::from_secs(60))
    }

    pub fn run_timeout(&self, args: &[&str], limit: Duration) -> (bool, String) {
        let mut child = Command::new(BIN)
            .arg("--home")
            .arg(&self.home)
            .args(args)
            .env("TENON_RELEASE_DIR", &self.release)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tenon");
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
        let output = child.wait_with_output().expect("collect output");
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), text)
    }

    pub fn spawn_attach(&self, extra: &[&str]) -> Child {
        let mut args = vec!["--home"];
        let home = self.home.to_string_lossy().to_string();
        args.push(&home);
        args.push("attach");
        args.extend_from_slice(extra);
        Command::new(BIN)
            .args(&args)
            .env("TENON_RELEASE_DIR", &self.release)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn attach")
    }

    pub fn start(&self, extra: &[&str]) {
        let mut args = vec!["start"];
        args.extend_from_slice(extra);
        let (ok, text) = self.run(&args);
        assert!(ok, "start failed: {text}\n{}", self.log());
    }

    pub fn status(&self) -> Value {
        let (ok, text) = self.run(&["status"]);
        assert!(ok, "status failed: {text}");
        serde_json::from_str(&text).expect("status json")
    }

    pub fn status_result(&self) -> Result<Value, String> {
        let (ok, text) = self.run_timeout(&["status"], Duration::from_secs(20));
        if !ok {
            return Err(text);
        }
        serde_json::from_str(&text).map_err(|error| error.to_string())
    }

    pub fn node(&self, env: &str) -> Value {
        let status = self.status();
        status["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|node| node["env"] == env)
            .cloned()
            .unwrap_or(Value::Null)
    }

    pub fn base_pid(&self) -> i64 {
        let path = self.home.join("run/base.ready");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pid) = text.trim().parse() {
                    return pid;
                }
            }
            if Instant::now() >= deadline {
                panic!("no valid pid in {}", path.display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn sock(&self) -> PathBuf {
        self.home.join("run/base.sock")
    }

    pub fn log(&self) -> String {
        ["base", "guardian", "root"]
            .iter()
            .map(|name| {
                let path = self.home.join(format!("run/{name}.log"));
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                format!("--- {name}.log\n{body}")
            })
            .collect()
    }

    pub fn await_fresh(&self, env: &str, old: i64) -> i64 {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let node = self.node(env);
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
            if let Ok(status) = self.status_result() {
                if pred(&status) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        false
    }

    /// The guardian and agent BEAM node processes, identified by the
    /// TENON_BASE_SOCK env var only `node::spawn` sets.
    pub fn node_pids(&self) -> Vec<i64> {
        pids_by_sock(&self.sock())
    }

    /// The base process(es) for this home, identified by `--home <home>` in
    /// their argv. More than one at a time means a double boot.
    pub fn base_pids(&self) -> Vec<i64> {
        pids_by_home(&self.home)
    }

    pub fn all_pids(&self) -> Vec<i64> {
        let mut pids = self.node_pids();
        pids.extend(self.base_pids());
        pids.sort_unstable();
        pids.dedup();
        pids
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.home.join("run/base.ready").is_file() {
            let _ = self.run_timeout(&["stop"], Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(500));
        }
        for pid in self.all_pids() {
            if alive(pid) {
                kill(pid, "-9");
            }
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

pub fn alive(pid: i64) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(tail) = stat.rsplit(')').next() else {
        return false;
    };
    !matches!(tail.split_whitespace().next(), Some("Z") | None)
}

pub fn wait_gone(pids: &[i64], limit: Duration) -> Duration {
    let started = Instant::now();
    while started.elapsed() < limit {
        if pids.iter().all(|pid| !alive(*pid)) {
            return started.elapsed();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    started.elapsed()
}

pub fn kill(pid: i64, signal: &str) {
    let _ = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status();
}

/// Every pid whose /proc/<pid>/environ names this fixture's socket path:
/// covers the guardian and agent BEAM nodes it spawned, no matter which
/// generation of `start` created them.
pub fn pids_by_sock(sock: &Path) -> Vec<i64> {
    let needle = format!("TENON_BASE_SOCK={}", sock.display());
    scan_proc(|pid| {
        std::fs::read(format!("/proc/{pid}/environ"))
            .map(|bytes| contains(&bytes, needle.as_bytes()))
            .unwrap_or(false)
    })
}

/// Every pid whose /proc/<pid>/cmdline names this fixture's home directory:
/// covers `tenon --home <home> start --foreground`, any generation.
pub fn pids_by_home(home: &Path) -> Vec<i64> {
    let needle = home.to_string_lossy().to_string();
    scan_proc(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| contains(&bytes, needle.as_bytes()))
            .unwrap_or(false)
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len().max(1))
        .any(|window| window == needle)
}

fn scan_proc(matches: impl Fn(i64) -> bool) -> Vec<i64> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<i64>() else {
            continue;
        };
        if matches(pid) {
            pids.push(pid);
        }
    }
    pids
}

pub fn raw_connect(sock: &Path) -> UnixStream {
    UnixStream::connect(sock).expect("connect raw socket")
}

pub fn send_raw(stream: &mut UnixStream, body: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn send_frame(stream: &mut UnixStream, frame: &Value) -> std::io::Result<()> {
    send_raw(stream, serde_json::to_vec(frame).unwrap().as_slice())
}

pub fn read_frame(stream: &mut UnixStream, timeout: Duration) -> std::io::Result<Value> {
    stream.set_read_timeout(Some(timeout))?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    let size = u32::from_be_bytes(head) as usize;
    let mut body = vec![0u8; size];
    stream.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}
