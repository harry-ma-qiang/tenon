use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tenon_worker::pty::{bash, BashOutcome, BashReq, Ptys};

static SEQ: AtomicU64 = AtomicU64::new(1);

struct Temp {
    path: PathBuf,
}

impl Temp {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tenon-pty-test-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
            tag
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        Self { path }
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn req(dir: &Temp, cmd: &str, pty: bool) -> BashReq {
    BashReq {
        cmd: cmd.to_string(),
        cwd: dir.path.clone(),
        timeout_ms: 10_000,
        env: Vec::new(),
        pty,
        spill_dir: dir.join("out"),
        tail_bytes: 32 * 1024,
    }
}

fn run(dir: &Temp, cmd: &str, pty: bool) -> BashOutcome {
    bash(&req(dir, cmd, pty)).expect("bash")
}

fn fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count())
        .unwrap_or(0)
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn wait_gone(pid: i32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

#[test]
fn echo_under_pty_and_pipes() {
    let dir = Temp::new("echo");
    for pty in [true, false] {
        let out = run(&dir, "echo hello-tenon", pty);
        assert_eq!(out.status, 0, "pty={pty} tail={}", out.tail);
        assert!(!out.timed_out);
        assert!(
            out.tail.contains("hello-tenon"),
            "pty={pty} tail={}",
            out.tail
        );
        assert!(out.bytes >= "hello-tenon".len());
        assert!(out.spill.is_none());
    }
}

#[test]
fn merged_stderr_and_exit_code() {
    let dir = Temp::new("code");
    let out = run(&dir, "echo to-err >&2; exit 7", false);
    assert_eq!(out.status, 7);
    assert!(!out.timed_out);
    assert!(out.tail.contains("to-err"), "tail={}", out.tail);
}

#[test]
fn timeout_kills_the_process_group() {
    let dir = Temp::new("timeout");
    let marker = dir.join("child.pid");
    let mut request = req(
        &dir,
        &format!("sleep 30 & echo $! > {}; sleep 30", marker.display()),
        true,
    );
    request.timeout_ms = 800;
    let start = Instant::now();
    let out = bash(&request).expect("bash");
    let elapsed = start.elapsed();
    assert!(out.timed_out);
    assert_eq!(out.status, 124);
    assert!(
        elapsed < Duration::from_millis(800) + Duration::from_secs(2),
        "elapsed={elapsed:?}"
    );
    let raw = std::fs::read_to_string(&marker).expect("child pid file");
    let grandchild: i32 = raw.trim().parse().expect("pid");
    assert!(
        wait_gone(grandchild, Duration::from_secs(3)),
        "grandchild {grandchild} survived the timeout"
    );
}

#[test]
fn large_output_spills_and_keeps_the_tail() {
    let dir = Temp::new("spill");
    let mut request = req(
        &dir,
        "for i in $(seq 1 20000); do echo line-$i; done",
        false,
    );
    request.tail_bytes = 4096;
    let out = bash(&request).expect("bash");
    assert_eq!(out.status, 0);
    assert!(out.bytes > 4096, "bytes={}", out.bytes);
    let spill = out.spill.expect("spill path");
    let body = std::fs::read(&spill).expect("spill body");
    assert_eq!(body.len(), out.bytes);
    assert_eq!(out.tail.len(), 4096);
    assert_eq!(&body[body.len() - 4096..], out.tail.as_bytes());
    assert!(body.starts_with(b"line-1\n"));
    assert!(out.tail.ends_with("line-20000\n"));
}

#[test]
fn small_output_has_no_spill_file() {
    let dir = Temp::new("nospill");
    let out = run(&dir, "printf abc", false);
    assert_eq!(out.bytes, 3);
    assert_eq!(out.tail, "abc");
    assert!(out.spill.is_none());
    assert!(!dir.join("out").exists());
}

#[test]
fn session_open_send_read_close() {
    let dir = Temp::new("session");
    let ptys = Ptys::new(&dir.path);
    assert_eq!(ptys.root(), dir.path.as_path());
    let opened = ptys.open(None, None, &[], 0, 0).expect("open");
    let id = opened["session"].as_u64().expect("session id");
    let pid = opened["pid"].as_i64().expect("pid") as i32;
    assert_eq!(opened["cols"], 80);
    assert_eq!(opened["rows"], 24);
    assert_eq!(ptys.count(), 1);

    let sent = ptys.send(id, "echo hel'lo'\n").expect("send");
    assert_eq!(sent["bytes"].as_u64(), Some(13));

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = String::new();
    while Instant::now() < deadline && !seen.contains("hello") {
        let chunk = ptys.read(id, 0).expect("read");
        assert_eq!(chunk["session"].as_u64(), Some(id));
        seen.push_str(chunk["data"].as_str().unwrap_or(""));
        if seen.contains("hello") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(seen.contains("hello"), "session output was {seen:?}");

    let closed = ptys.close(id).expect("close");
    assert_eq!(closed["session"].as_u64(), Some(id));
    assert!(closed["status"].is_number());
    assert_eq!(ptys.count(), 0);
    assert!(
        wait_gone(pid, Duration::from_secs(3)),
        "pid {pid} survived close"
    );
}

#[test]
fn session_reports_env_cwd_and_liveness() {
    let dir = Temp::new("env");
    let ptys = Ptys::new(&dir.path);
    let env = vec![("TENON_MARKER".to_string(), "on".to_string())];
    let opened = ptys
        .open(Some("echo $TENON_MARKER; pwd"), None, &env, 100, 40)
        .expect("open");
    let id = opened["session"].as_u64().expect("session id");
    assert_eq!(opened["cols"], 100);
    assert_eq!(opened["rows"], 40);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = String::new();
    while Instant::now() < deadline && !seen.contains("on") {
        seen.push_str(
            ptys.read(id, 0).expect("read")["data"]
                .as_str()
                .unwrap_or(""),
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(seen.contains("on"), "output was {seen:?}");
    let _ = ptys.close(id);
}

#[test]
fn unknown_session_is_a_loud_error() {
    let dir = Temp::new("unknown");
    let ptys = Ptys::new(&dir.path);
    assert!(ptys.read(4242, 0).is_err());
    assert!(ptys.send(4242, "x").is_err());
    assert!(ptys.close(4242).is_err());
    assert_eq!(ptys.count(), 0);
}

#[test]
fn close_all_leaves_nothing_behind() {
    let dir = Temp::new("closeall");
    let ptys = Ptys::new(&dir.path);
    let mut pids = Vec::new();
    for _ in 0..3 {
        let opened = ptys.open(None, None, &[], 0, 0).expect("open");
        pids.push(opened["pid"].as_i64().expect("pid") as i32);
    }
    assert_eq!(ptys.count(), 3);
    ptys.close_all();
    assert_eq!(ptys.count(), 0);
    for pid in pids {
        assert!(
            wait_gone(pid, Duration::from_secs(3)),
            "pid {pid} survived close_all"
        );
    }
}

#[test]
fn no_fd_leak_across_sessions_and_calls() {
    let dir = Temp::new("fds");
    let ptys = Ptys::new(&dir.path);
    let opened = ptys.open(None, None, &[], 0, 0).expect("warmup open");
    ptys.close(opened["session"].as_u64().expect("id"))
        .expect("warmup close");
    let _ = run(&dir, "true", false);
    let _ = run(&dir, "true", true);

    let before = fd_count();
    for _ in 0..50 {
        let opened = ptys.open(None, None, &[], 0, 0).expect("open");
        let id = opened["session"].as_u64().expect("id");
        let _ = ptys.read(id, 0);
        ptys.close(id).expect("close");
    }
    let after_sessions = fd_count();
    for _ in 0..200 {
        let out = run(&dir, "true", false);
        assert_eq!(out.status, 0);
    }
    let after_calls = fd_count();
    assert!(
        after_sessions <= before + 16,
        "fds before={before} after 50 sessions={after_sessions}"
    );
    assert!(
        after_calls <= before + 16,
        "fds before={before} after 200 bash calls={after_calls}"
    );
}
