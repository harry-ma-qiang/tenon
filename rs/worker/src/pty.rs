use crate::{err, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL_MS: i32 = 20;
const CHUNK: usize = 16 * 1024;
const GRACE: Duration = Duration::from_millis(500);
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_READ_MAX: usize = 64 * 1024;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const TIMEOUT_STATUS: i32 = 124;

static SEQ: AtomicU64 = AtomicU64::new(1);

pub struct BashReq {
    pub cmd: String,
    pub cwd: PathBuf,
    pub timeout_ms: u64,
    pub env: Vec<(String, String)>,
    pub pty: bool,
    pub spill_dir: PathBuf,
    pub tail_bytes: usize,
}

pub struct BashOutcome {
    pub status: i32,
    pub timed_out: bool,
    pub bytes: usize,
    pub tail: String,
    pub spill: Option<PathBuf>,
}

pub fn bash(req: &BashReq) -> Result<BashOutcome> {
    let cap = if req.tail_bytes == 0 {
        crate::TAIL_BYTES
    } else {
        req.tail_bytes
    };
    let timeout = Duration::from_millis(if req.timeout_ms == 0 {
        DEFAULT_TIMEOUT_MS
    } else {
        req.timeout_ms
    });
    let (shell, login) = shell();
    let mut command = Command::new(shell);
    command.arg(login).arg(&req.cmd).current_dir(&req.cwd);
    if req.pty {
        command.env("TERM", "dumb");
    }
    command.envs(req.env.iter().map(|(key, value)| (key, value)));

    let (mut child, source) = if req.pty {
        let (master, slave) = openpty(DEFAULT_COLS, DEFAULT_ROWS)?;
        command.stdin(Stdio::from(slave.try_clone()?));
        command.stdout(Stdio::from(slave.try_clone()?));
        command.stderr(Stdio::from(slave));
        session_leader(&mut command);
        let child = command.spawn()?;
        (child, master)
    } else {
        let (reader, writer) = pipe()?;
        command.stdin(Stdio::null());
        command.stdout(Stdio::from(writer.try_clone()?));
        command.stderr(Stdio::from(writer));
        command.process_group(0);
        let child = command.spawn()?;
        (child, reader)
    };
    drop(command);

    let fd = source.as_raw_fd();
    nonblocking(fd);
    let mut sink = Sink::new(cap, req.spill_dir.clone());
    let watched = watch(&mut child, fd, &mut sink, Instant::now() + timeout);
    let (status, timed_out) = match watched {
        Ok(pair) => pair,
        Err(error) => {
            stop_group(&mut child);
            return Err(error);
        }
    };
    let (bytes, tail, spill) = sink.finish();
    Ok(BashOutcome {
        status,
        timed_out,
        bytes,
        tail,
        spill,
    })
}

fn watch(child: &mut Child, fd: RawFd, sink: &mut Sink, deadline: Instant) -> Result<(i32, bool)> {
    let mut eof = false;
    let mut timed_out = false;
    let mut status = 0;
    loop {
        if eof {
            thread::sleep(Duration::from_millis(POLL_MS as u64));
        } else if readable(fd, POLL_MS) {
            eof = drain(fd, sink)?;
        }
        if let Some(done) = child.try_wait()? {
            status = code_of(done);
            break;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
    }
    if timed_out {
        stop_group(child);
        status = TIMEOUT_STATUS;
    }
    for _ in 0..3 {
        if eof || !readable(fd, POLL_MS) {
            break;
        }
        eof = drain(fd, sink)?;
    }
    Ok((status, timed_out))
}

struct Sink {
    cap: usize,
    dir: PathBuf,
    tail: Vec<u8>,
    bytes: usize,
    spill: Option<(PathBuf, File)>,
}

impl Sink {
    fn new(cap: usize, dir: PathBuf) -> Self {
        Self {
            cap,
            dir,
            tail: Vec::new(),
            bytes: 0,
            spill: None,
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<()> {
        self.bytes += chunk.len();
        if self.spill.is_none() && self.bytes > self.cap {
            std::fs::create_dir_all(&self.dir)?;
            let path = self.dir.join(format!(
                "bash-{}-{}.out",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = File::create(&path)?;
            file.write_all(&self.tail)?;
            self.spill = Some((path, file));
        }
        if let Some((_, file)) = self.spill.as_mut() {
            file.write_all(chunk)?;
        }
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > self.cap {
            let excess = self.tail.len() - self.cap;
            self.tail.drain(..excess);
        }
        Ok(())
    }

    fn finish(mut self) -> (usize, String, Option<PathBuf>) {
        let spill = match self.spill.take() {
            Some((path, mut file)) => {
                let _ = file.flush();
                Some(path)
            }
            None => None,
        };
        (
            self.bytes,
            String::from_utf8_lossy(&self.tail).into_owned(),
            spill,
        )
    }
}

struct Ring {
    buf: Vec<u8>,
    dropped: usize,
}

impl Ring {
    fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > crate::RING_BYTES {
            let excess = self.buf.len() - crate::RING_BYTES;
            self.buf.drain(..excess);
            self.dropped += excess;
        }
    }

    fn take(&mut self, max: usize) -> (Vec<u8>, usize) {
        let count = max.min(self.buf.len());
        let data = self.buf.drain(..count).collect();
        (data, std::mem::take(&mut self.dropped))
    }
}

struct Session {
    child: Child,
    master: OwnedFd,
    ring: Arc<Mutex<Ring>>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

pub struct Ptys {
    root: PathBuf,
    next: AtomicU64,
    sessions: Mutex<HashMap<u64, Session>>,
}

impl Ptys {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            next: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn open(
        &self,
        cmd: Option<&str>,
        cwd: Option<&str>,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> Result<Value> {
        let cols = if cols == 0 { DEFAULT_COLS } else { cols };
        let rows = if rows == 0 { DEFAULT_ROWS } else { rows };
        let (master, slave) = openpty(cols, rows)?;
        let (shell, _) = shell();
        let mut command = Command::new(shell);
        match cmd.filter(|line| !line.trim().is_empty()) {
            Some(line) => {
                command.arg("-c").arg(line);
            }
            None => {
                command.arg("-i");
            }
        }
        command
            .current_dir(cwd.filter(|dir| !dir.is_empty()).map_or_else(
                || self.root.to_path_buf(),
                |dir| Path::new(dir).to_path_buf(),
            ))
            .env("TERM", "xterm-256color")
            .envs(env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        session_leader(&mut command);
        let child = command.spawn()?;
        drop(command);

        let pid = child.id() as i32;
        let ring = Arc::new(Mutex::new(Ring {
            buf: Vec::new(),
            dropped: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let pump_fd = master.try_clone()?;
        let pump_ring = Arc::clone(&ring);
        let pump_stop = Arc::clone(&stop);
        let reader = thread::spawn(move || pump(pump_fd, &pump_ring, &pump_stop));
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let session = Session {
            child,
            master,
            ring,
            stop,
            reader: Some(reader),
        };
        self.sessions
            .lock()
            .map_err(|_| err("pty sessions poisoned"))?
            .insert(id, session);
        Ok(json!({"session": id, "pid": pid, "cols": cols, "rows": rows}))
    }

    pub fn send(&self, id: u64, data: &str) -> Result<Value> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| err("pty sessions poisoned"))?;
        let session = sessions
            .get_mut(&id)
            .ok_or_else(|| err(format!("unknown pty session {id}")))?;
        let body = data.as_bytes();
        let mut written = 0;
        while written < body.len() {
            let count = unsafe {
                libc::write(
                    session.master.as_raw_fd(),
                    body[written..].as_ptr().cast(),
                    body.len() - written,
                )
            };
            if count > 0 {
                written += count as usize;
                continue;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    thread::sleep(Duration::from_millis(POLL_MS as u64));
                    continue;
                }
                _ => return Err(err(format!("pty session {id} write: {error}"))),
            }
        }
        Ok(json!({"session": id, "bytes": written}))
    }

    pub fn read(&self, id: u64, max: usize) -> Result<Value> {
        let max = if max == 0 { DEFAULT_READ_MAX } else { max };
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| err("pty sessions poisoned"))?;
        let session = sessions
            .get_mut(&id)
            .ok_or_else(|| err(format!("unknown pty session {id}")))?;
        let alive = session.child.try_wait()?.is_none();
        let (data, dropped) = session
            .ring
            .lock()
            .map_err(|_| err("pty ring poisoned"))?
            .take(max);
        Ok(json!({
            "session": id,
            "data": String::from_utf8_lossy(&data),
            "bytes": data.len(),
            "dropped": dropped,
            "alive": alive
        }))
    }

    pub fn close(&self, id: u64) -> Result<Value> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| err("pty sessions poisoned"))?
            .remove(&id)
            .ok_or_else(|| err(format!("unknown pty session {id}")))?;
        let status = shutdown(session);
        Ok(json!({"session": id, "status": status}))
    }

    pub fn close_all(&self) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        for (_, session) in sessions.drain() {
            shutdown(session);
        }
    }

    pub fn count(&self) -> usize {
        self.sessions.lock().map(|held| held.len()).unwrap_or(0)
    }
}

impl Drop for Ptys {
    fn drop(&mut self) {
        self.close_all();
    }
}

fn shutdown(mut session: Session) -> i32 {
    let status = stop_group(&mut session.child);
    session.stop.store(true, Ordering::Relaxed);
    if let Some(reader) = session.reader.take() {
        let _ = reader.join();
    }
    status
}

fn pump(fd: OwnedFd, ring: &Arc<Mutex<Ring>>, stop: &Arc<AtomicBool>) {
    let raw = fd.as_raw_fd();
    nonblocking(raw);
    let mut buf = [0u8; CHUNK];
    while !stop.load(Ordering::Relaxed) {
        if !readable(raw, POLL_MS) {
            continue;
        }
        let count = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
        if count > 0 {
            if let Ok(mut held) = ring.lock() {
                held.push(&buf[..count as usize]);
            }
            continue;
        }
        if count == 0 {
            break;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
            _ => break,
        }
    }
}

fn stop_group(child: &mut Child) -> i32 {
    let pid = child.id() as i32;
    if let Ok(Some(done)) = child.try_wait() {
        return code_of(done);
    }
    unsafe { libc::killpg(pid, libc::SIGTERM) };
    let deadline = Instant::now() + GRACE;
    while Instant::now() < deadline {
        if let Ok(Some(done)) = child.try_wait() {
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            return code_of(done);
        }
        thread::sleep(Duration::from_millis(POLL_MS as u64));
    }
    unsafe { libc::killpg(pid, libc::SIGKILL) };
    child.wait().map(code_of).unwrap_or(-1)
}

fn drain(fd: RawFd, sink: &mut Sink) -> Result<bool> {
    let mut buf = [0u8; CHUNK];
    loop {
        let count = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if count > 0 {
            sink.push(&buf[..count as usize])?;
            continue;
        }
        if count == 0 {
            return Ok(true);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(false),
            _ => return Ok(true),
        }
    }
}

fn openpty(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd)> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master = -1;
    let mut slave = -1;
    let made = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    if made != 0 {
        return Err(err(format!("openpty: {}", std::io::Error::last_os_error())));
    }
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)))
    }
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut ends = [-1; 2];
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(err(format!("pipe: {}", std::io::Error::last_os_error())));
    }
    unsafe { Ok((OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1]))) }
}

// why: stdio is already dup'd from the slave when pre_exec runs, so fd 0 is the pty to claim.
fn session_leader(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn shell() -> (&'static str, &'static str) {
    if Path::new("/bin/bash").is_file() {
        ("/bin/bash", "-lc")
    } else {
        ("/bin/sh", "-c")
    }
}

fn nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
}

fn readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut watch = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut watch, 1, timeout_ms) > 0 }
}

fn code_of(status: ExitStatus) -> i32 {
    match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(0),
    }
}
