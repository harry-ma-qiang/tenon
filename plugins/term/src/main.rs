use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};
use tenon_sdk::{arg, handler, Error, Handler, Next, Plugin, Result};

const INLINE_LIMIT: usize = 65_536;
const KILLED_ON_TIMEOUT: i64 = -1;
const POLL: Duration = Duration::from_millis(5);

struct Term {
    dir: PathBuf,
    children: HashMap<u32, Child>,
    inline_cap: usize,
    seq: u64,
}

type Shared = Rc<RefCell<Term>>;

impl Term {
    fn path(&mut self, prefix: &str, suffix: &str) -> PathBuf {
        self.seq += 1;
        self.dir.join(format!("{prefix}-{}.{suffix}", self.seq))
    }

    fn reaped(&mut self) -> Vec<(u32, i64)> {
        let mut done = Vec::new();
        self.children.retain(|pid, child| match child.try_wait() {
            Ok(Some(status)) => {
                done.push((*pid, code_of(status)));
                false
            }
            Ok(None) => true,
            Err(_) => {
                done.push((*pid, KILLED_ON_TIMEOUT));
                false
            }
        });
        done
    }
}

fn main() {
    let state: Shared = Rc::new(RefCell::new(Term {
        dir: PathBuf::new(),
        children: HashMap::new(),
        inline_cap: INLINE_LIMIT,
        seq: 0,
    }));
    let mut plugin = Plugin::new(&[]);

    let loaded = state.clone();
    plugin.on_load(move |config: Value, next: &mut Next| {
        let dir = workspace()?;
        {
            let mut term = loaded.borrow_mut();
            term.dir = dir.clone();
            term.inline_cap = INLINE_LIMIT.min(next.max_frame() / 8).max(1024);
        }
        let name = config["service"].as_str().unwrap_or("term").to_string();
        next.provide(&name, methods(loaded.clone()));
        next.log(format!(
            "term plugin loaded as {name}, handles in {}",
            dir.display()
        ));
        Ok(())
    });

    let unloaded = state.clone();
    plugin.on_unload(move |next: &mut Next| {
        let mut term = unloaded.borrow_mut();
        let pids: Vec<u32> = term.children.keys().copied().collect();
        for pid in pids {
            if let Some(mut child) = term.children.remove(&pid) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let dir = term.dir.clone();
        drop(term);
        if dir.as_os_str().is_empty() {
            return Ok(());
        }
        let _ = fs::remove_dir_all(&dir);
        next.log(format!("term plugin unloaded, removed {}", dir.display()));
        Ok(())
    });

    plugin.run()
}

fn methods(state: Shared) -> HashMap<&'static str, Handler> {
    let mut methods: HashMap<&'static str, Handler> = HashMap::new();
    let exec_state = state.clone();
    methods.insert(
        "exec",
        handler(move |args: Vec<Value>, next: &mut Next| exec(&exec_state, args, next)),
    );
    let spawn_state = state.clone();
    methods.insert(
        "spawn",
        handler(move |args: Vec<Value>, next: &mut Next| spawn(&spawn_state, args, next)),
    );
    let kill_state = state.clone();
    methods.insert(
        "kill",
        handler(move |args: Vec<Value>, next: &mut Next| kill(&kill_state, args, next)),
    );
    let read_state = state.clone();
    methods.insert(
        "read",
        handler(move |args: Vec<Value>, next: &mut Next| read(&read_state, args, next)),
    );
    let ping_state = state;
    methods.insert(
        "ping",
        handler(move |_args: Vec<Value>, next: &mut Next| {
            reap(&ping_state, next)?;
            Ok(json!("pong"))
        }),
    );
    methods.insert(
        "pid",
        handler(|_args: Vec<Value>, _next: &mut Next| Ok(json!(std::process::id()))),
    );
    methods
}

// The wire loop is single threaded, so exits are noticed on the next request and
// announced from the loop itself; nothing else writes to the wire.
fn reap(state: &Shared, next: &mut Next) -> Result<()> {
    let done = state.borrow_mut().reaped();
    for (pid, status) in done {
        next.emit("term/exit", vec![json!({"pid": pid, "status": status})])?;
    }
    Ok(())
}

fn exec(state: &Shared, args: Vec<Value>, next: &mut Next) -> Result<Value> {
    reap(state, next)?;
    let mut command = Command::new(program(&args)?);
    command
        .args(argv(&args))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = arg(&args, 2).as_str().filter(|dir| !dir.is_empty()) {
        command.current_dir(cwd);
    }
    let timeout = arg(&args, 3).as_u64();
    let mut child = command.spawn()?;
    let mut out = child.stdout.take().ok_or_else(|| Error::msg("no stdout"))?;
    let mut err = child.stderr.take().ok_or_else(|| Error::msg("no stderr"))?;
    let out_reader = thread::spawn(move || drain(&mut out));
    let err_reader = thread::spawn(move || drain(&mut err));
    let status = wait_for(&mut child, timeout)?;
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    let (stdout, spilled_out) = stream(state, "stdout", &stdout)?;
    let (stderr, spilled_err) = stream(state, "stderr", &stderr)?;
    Ok(json!({
        "status": status,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": spilled_out || spilled_err
    }))
}

fn spawn(state: &Shared, args: Vec<Value>, next: &mut Next) -> Result<Value> {
    reap(state, next)?;
    let log = state.borrow_mut().path("spawn", "log");
    let sink = File::create(&log)?;
    let child = Command::new(program(&args)?)
        .args(argv(&args))
        .stdin(Stdio::null())
        .stdout(Stdio::from(sink.try_clone()?))
        .stderr(Stdio::from(sink))
        .spawn()?;
    let pid = child.id();
    state.borrow_mut().children.insert(pid, child);
    Ok(json!({"pid": pid, "log": log.to_string_lossy()}))
}

fn kill(state: &Shared, args: Vec<Value>, next: &mut Next) -> Result<Value> {
    reap(state, next)?;
    let pid = arg(&args, 0)
        .as_u64()
        .ok_or_else(|| Error::msg("kill needs a pid"))? as u32;
    let child = state.borrow_mut().children.remove(&pid);
    let Some(mut child) = child else {
        return Err(Error::msg(format!("unknown pid {pid}")));
    };
    child.kill()?;
    let status = code_of(child.wait()?);
    next.emit("term/exit", vec![json!({"pid": pid, "status": status})])?;
    Ok(json!({"pid": pid, "status": status}))
}

fn read(state: &Shared, args: Vec<Value>, next: &mut Next) -> Result<Value> {
    let handle = arg(&args, 0)
        .as_str()
        .ok_or_else(|| Error::msg("read needs a handle"))?
        .to_string();
    let offset = arg(&args, 1).as_u64().unwrap_or(0);
    let cap = state
        .borrow()
        .inline_cap
        .min(next.max_frame() / 8)
        .max(1024);
    let len = arg(&args, 2).as_u64().unwrap_or(cap as u64).min(cap as u64);
    let mut file = File::open(&handle)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut body = Vec::new();
    file.take(len).read_to_end(&mut body)?;
    Ok(json!(String::from_utf8_lossy(&body)))
}

fn stream(state: &Shared, kind: &str, body: &[u8]) -> Result<(Value, bool)> {
    let inline = state.borrow().inline_cap;
    if body.len() <= inline {
        if let Ok(text) = std::str::from_utf8(body) {
            return Ok((json!(text), false));
        }
    }
    let path = state.borrow_mut().path(kind, "bin");
    File::create(&path)?.write_all(body)?;
    Ok((
        json!({"handle": path.to_string_lossy(), "bytes": body.len()}),
        true,
    ))
}

fn wait_for(child: &mut Child, timeout_ms: Option<u64>) -> Result<i64> {
    let Some(timeout_ms) = timeout_ms else {
        return Ok(code_of(child.wait()?));
    };
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(code_of(status));
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait()?;
            return Ok(KILLED_ON_TIMEOUT);
        }
        thread::sleep(POLL);
    }
}

fn drain(source: &mut impl Read) -> Vec<u8> {
    let mut body = Vec::new();
    let _ = source.read_to_end(&mut body);
    body
}

fn program(args: &[Value]) -> Result<String> {
    arg(args, 0)
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| Error::msg("first argument must be the command"))
}

fn argv(args: &[Value]) -> Vec<String> {
    arg(args, 1)
        .as_array()
        .map(|items| items.iter().map(text).collect())
        .unwrap_or_default()
}

fn text(value: &Value) -> String {
    match value {
        Value::String(item) => item.clone(),
        other => other.to_string(),
    }
}

fn code_of(status: ExitStatus) -> i64 {
    match status.code() {
        Some(code) => code as i64,
        None => 128 + status.signal().unwrap_or(0) as i64,
    }
}

fn workspace() -> Result<PathBuf> {
    let root = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    let dir = Path::new(&root).join(format!("tenon-term-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}
