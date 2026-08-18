use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tenon");
static SEQ: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    dir: PathBuf,
    child: Child,
    wire: UnixStream,
    next: u64,
}

impl Fixture {
    fn start(max_frame: Option<&str>) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tenon-it-{}-worker-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("workspace")).unwrap();
        let sock = dir.join("gateway.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let mut command = Command::new(BIN);
        command
            .arg("worker")
            .arg("--workspace")
            .arg(dir.join("workspace"))
            .env("TENON_GATEWAY", format!("unix:{}", sock.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(cap) = max_frame {
            command.env("TENON_MAX_FRAME", cap);
        }
        let child = command.spawn().expect("spawn tenon worker");
        let (wire, _address) = listener.accept().expect("worker connects to the gateway");
        wire.set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let mut fixture = Fixture {
            dir,
            child,
            wire,
            next: 0,
        };
        let hello = fixture.read();
        assert_eq!(hello["t"], "hello", "{hello}");
        let req = fixture.alloc();
        fixture.write(&json!({"t": "load", "req": req, "config": {}}));
        let mut provided = false;
        loop {
            let frame = fixture.read();
            match frame["t"].as_str() {
                Some("provide") => provided = frame["name"] == "worker",
                Some("rep") if frame["req"] == req => break,
                _ => continue,
            }
        }
        assert!(provided, "the worker never provided the worker service");
        fixture
    }

    fn workspace(&self) -> PathBuf {
        self.dir.join("workspace")
    }

    fn alloc(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn write(&mut self, frame: &Value) {
        let body = serde_json::to_vec(frame).unwrap();
        self.wire
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        self.wire.write_all(&body).unwrap();
        self.wire.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut head = [0u8; 4];
        self.wire.read_exact(&mut head).expect("read frame header");
        let size = u32::from_be_bytes(head) as usize;
        let mut body = vec![0u8; size];
        self.wire.read_exact(&mut body).expect("read frame body");
        serde_json::from_slice(&body).expect("frame is json")
    }

    fn svc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let req = self.alloc();
        self.write(&json!({
            "t": "svc", "req": req, "name": "worker", "method": method, "args": [params]
        }));
        loop {
            let frame = self.read();
            if frame["t"] == "rep" && frame["req"] == req {
                return match frame.get("error") {
                    Some(error) => Err(error.as_str().unwrap_or("error").to_string()),
                    None => Ok(frame["result"].clone()),
                };
            }
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.svc(method, params)
            .unwrap_or_else(|error| panic!("{method}: {error}"))
    }

    fn fds(&self) -> usize {
        std::fs::read_dir(format!("/proc/{}/fd", self.child.id()))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn the_worker_serves_its_tools_over_a_gateway_socket() {
    let mut worker = Fixture::start(None);

    assert_eq!(worker.call("ping", json!({})), json!("pong"));
    let info = worker.call("info", json!({}));
    assert_eq!(info["workspace"], worker.workspace().display().to_string());

    let echoed = worker.call("bash", json!({"cmd": "echo wired", "timeout_ms": 20000}));
    assert_eq!(echoed["status"], 0, "{echoed}");
    assert!(
        echoed["tail"]
            .as_str()
            .unwrap_or_default()
            .contains("wired"),
        "{echoed}"
    );

    let written = worker.call(
        "fs.write",
        json!({"path": "a.txt", "content": "one\ntwo\n"}),
    );
    assert_eq!(written["bytes"], 8, "{written}");
    let viewed = worker.call("fs.view", json!({"path": "a.txt"}));
    assert!(
        viewed["content"]
            .as_str()
            .unwrap_or_default()
            .contains("two"),
        "{viewed}"
    );

    worker.call("fs.write", json!({"path": "b.txt", "content": "x\nx\n"}));
    let error = worker
        .svc("fs.edit", json!({"path": "b.txt", "old": "x", "new": "y"}))
        .expect_err("two matches must fail loud");
    assert!(error.contains("unique"), "{error}");

    let committed = worker.call("snap.commit", json!({"label": "first"}));
    assert_eq!(committed["step"], 1, "{committed}");
    let listed = worker.call("snap.list", json!({}));
    assert_eq!(listed["count"], 1, "{listed}");

    let packed = worker.call("snap.pack", json!({"since": 0}));
    assert_eq!(packed["step"], 1, "{packed}");
    assert!(
        packed["pack"].as_str().unwrap_or_default().len() > 16,
        "{packed}"
    );

    let session = worker.call("pty.open", json!({}));
    let id = session["session"].as_u64().expect("session id");
    worker.call(
        "pty.send",
        json!({"session": id, "data": "echo ptyhello\n"}),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen = String::new();
    while Instant::now() < deadline && !seen.contains("ptyhello") {
        let read = worker.call("pty.read", json!({"session": id}));
        seen.push_str(read["data"].as_str().unwrap_or_default());
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen.contains("ptyhello"), "pty session said {seen:?}");
    worker.call("pty.close", json!({ "session": id }));
}

#[test]
fn an_answer_over_the_frame_cap_comes_back_as_a_handle() {
    let mut worker = Fixture::start(Some("65536"));
    let body = "0123456789abcdef\n".repeat(2000);
    worker.call("fs.write", json!({"path": "big.txt", "content": body}));
    let viewed = worker.call(
        "fs.view",
        json!({"path": "big.txt", "start": 1, "end": 2000}),
    );
    assert_eq!(viewed["over_cap"], true, "{viewed}");
    let handle = viewed["handle"].as_str().expect("handle path");
    assert!(
        handle.starts_with(&worker.workspace().display().to_string()),
        "{handle} is not inside the workspace"
    );
    assert!(std::fs::metadata(handle).unwrap().len() > 32_000);
}

#[test]
fn five_hundred_steps_leak_nothing_and_expiry_keeps_the_count_bounded() {
    let mut worker = Fixture::start(None);
    worker.call(
        "fs.write",
        json!({"path": ".gitignore", "content": "junk/\n"}),
    );
    let fds_before = worker.fds();
    let mut since = 0u64;
    for step in 0..500 {
        worker.call(
            "fs.write",
            json!({"path": "loop.txt", "content": format!("step {step}\n")}),
        );
        let committed = worker.call("snap.commit", json!({}));
        assert_eq!(committed["step"], step + 1, "{committed}");
        let packed = worker.call("snap.pack", json!({ "since": since }));
        assert_eq!(packed["step"], step + 1, "{packed}");
        since = packed["step"].as_u64().unwrap();
        if step % 50 == 0 {
            worker.call(
                "snap.expire",
                json!({"keep_last": 10, "milestone_every": 100}),
            );
        }
    }
    let expired = worker.call(
        "snap.expire",
        json!({"keep_last": 10, "milestone_every": 100}),
    );
    let listed = worker.call("snap.list", json!({}));
    let count = listed["count"].as_u64().unwrap();
    assert!(count <= 20, "expiry left {count} snapshots: {expired}");
    let fds_after = worker.fds();
    assert!(
        fds_after <= fds_before + 4,
        "fds went from {fds_before} to {fds_after}"
    );
}
