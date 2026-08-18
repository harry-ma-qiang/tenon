use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::net::UnixStream;
use std::process;
use std::rc::Rc;

const WIRE_IN_FD: i32 = 3;
const WIRE_OUT_FD: i32 = 4;
const DEFAULT_MAX_FRAME: usize = 1_048_576;
const DEFAULT_DEADLINE_MS: u64 = 30_000;
const NULL: Value = Value::Null;

pub type Result<T> = std::result::Result<T, Error>;
pub type Handler = Rc<dyn Fn(Vec<Value>, &mut Next) -> Result<Value>>;

#[derive(Debug)]
pub enum Error {
    Wire(String),
    FrameTooLarge { size: usize, cap: usize },
    Remote(String),
    Failed(String),
    Disconnected,
    Unloaded,
}

impl Error {
    pub fn msg(text: impl Into<String>) -> Self {
        Error::Failed(text.into())
    }

    fn fatal(&self) -> bool {
        matches!(self, Error::Disconnected | Error::Unloaded)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Wire(text) | Error::Remote(text) | Error::Failed(text) => f.write_str(text),
            Error::FrameTooLarge { .. } => f.write_str("frame_too_large"),
            Error::Disconnected => f.write_str("wire closed"),
            Error::Unloaded => f.write_str("unloaded"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Wire(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::Wire(error.to_string())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Emit,
    Call,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Emit => "emit",
            Mode::Call => "call",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Slot {
    Rep,
    Result,
}

pub fn handler<F>(body: F) -> Handler
where
    F: Fn(Vec<Value>, &mut Next) -> Result<Value> + 'static,
{
    Rc::new(body)
}

pub fn arg(args: &[Value], index: usize) -> &Value {
    args.get(index).unwrap_or(&NULL)
}

fn env_int(name: &str, fallback: u64) -> u64 {
    match std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(value) if value > 0 => value,
        _ => fallback,
    }
}

struct Hook {
    mode: Mode,
    handler: Handler,
}

pub struct Plugin {
    inject: Vec<String>,
    max_frame: usize,
    deadline_ms: u64,
    config: Value,
    reader: BufReader<File>,
    writer: File,
    hooks: HashMap<u64, Hook>,
    services: HashMap<String, HashMap<String, Handler>>,
    replies: HashMap<(Slot, u64), Result<Value>>,
    deferred: Vec<Value>,
    load_handler: Option<Handler>,
    unload_handler: Option<Handler>,
    seq: u64,
    active: bool,
    stopped: bool,
}

/// The two wire ends of a plugin: fd 3/4 when the host spawned us directly,
/// a connection to `TENON_GATEWAY` when we were started inside a sandbox and
/// have to dial the gateway plugin in our node instead (RFC section 6).
pub fn wires() -> Result<(File, File)> {
    match std::env::var("TENON_GATEWAY") {
        Ok(address) if !address.trim().is_empty() => connect(address.trim()),
        _ => Ok(unsafe {
            (
                File::from_raw_fd(WIRE_IN_FD),
                File::from_raw_fd(WIRE_OUT_FD),
            )
        }),
    }
}

pub fn connect(address: &str) -> Result<(File, File)> {
    let fd = if let Some(path) = address.strip_prefix("unix:") {
        UnixStream::connect(path)
            .map_err(|error| Error::Wire(format!("connect {path}: {error}")))?
            .into_raw_fd()
    } else if let Some(rest) = address.strip_prefix("tcp:") {
        TcpStream::connect(rest)
            .map_err(|error| Error::Wire(format!("connect {rest}: {error}")))?
            .into_raw_fd()
    } else {
        return Err(Error::Wire(format!("bad TENON_GATEWAY address: {address}")));
    };
    let write = unsafe { libc::dup(fd) };
    if write < 0 {
        return Err(Error::Wire("dup of the gateway socket failed".to_string()));
    }
    Ok(unsafe { (File::from_raw_fd(fd), File::from_raw_fd(write)) })
}

impl Plugin {
    /// Panics only through `exit(1)`: a plugin that cannot reach its wire has
    /// nothing to report to and nobody to report it to but stderr.
    pub fn new(inject: &[&str]) -> Self {
        match Self::try_new(inject) {
            Ok(plugin) => plugin,
            Err(error) => {
                eprintln!("tenon: no wire: {error}");
                process::exit(1)
            }
        }
    }

    pub fn try_new(inject: &[&str]) -> Result<Self> {
        let (reader, writer) = wires()?;
        Ok(Self::with_wires(inject, reader, writer))
    }

    pub fn with_wires(inject: &[&str], reader: File, writer: File) -> Self {
        Plugin {
            inject: inject.iter().map(|name| name.to_string()).collect(),
            max_frame: env_int("TENON_MAX_FRAME", DEFAULT_MAX_FRAME as u64) as usize,
            deadline_ms: env_int("TENON_KERNEL_DEADLINE", DEFAULT_DEADLINE_MS),
            config: Value::Null,
            reader: BufReader::new(reader),
            writer,
            hooks: HashMap::new(),
            services: HashMap::new(),
            replies: HashMap::new(),
            deferred: Vec::new(),
            load_handler: None,
            unload_handler: None,
            seq: 0,
            active: false,
            stopped: false,
        }
    }

    pub fn max_frame(&self) -> usize {
        self.max_frame
    }

    pub fn deadline_ms(&self) -> u64 {
        self.deadline_ms
    }

    pub fn config(&self) -> &Value {
        &self.config
    }

    pub fn log(&self, message: impl fmt::Display) {
        let mut err = io::stderr();
        let _ = writeln!(err, "{message}");
        let _ = err.flush();
    }

    pub fn on(&mut self, event: &str, mode: Mode, prepend: bool, arity: u64, body: Handler) -> u64 {
        let hook = self.alloc();
        self.hooks.insert(
            hook,
            Hook {
                mode,
                handler: body,
            },
        );
        self.register(json!({
            "t": "on", "hook": hook, "event": event,
            "arity": arity, "mode": mode.as_str(), "prepend": prepend
        }));
        hook
    }

    pub fn off(&mut self, hook: u64) {
        self.hooks.remove(&hook);
        self.register(json!({"t": "off", "hook": hook}));
    }

    pub fn provide(&mut self, name: &str, methods: HashMap<&str, Handler>) {
        let owned = methods
            .into_iter()
            .map(|(method, body)| (method.to_string(), body))
            .collect();
        self.services.insert(name.to_string(), owned);
        self.register(json!({"t": "provide", "name": name}));
    }

    pub fn unprovide(&mut self, name: &str) {
        self.services.remove(name);
        self.register(json!({"t": "unprovide", "name": name}));
    }

    pub fn emit(&mut self, event: &str, args: Vec<Value>) -> Result<()> {
        self.send(&json!({"t": "emit", "event": event, "args": args}))
    }

    pub fn call(&mut self, event: &str, args: Vec<Value>) -> Result<Value> {
        let id = self.alloc();
        self.send(&json!({"t": "call", "id": id, "event": event, "args": args}))?;
        self.settle((Slot::Rep, id))
    }

    pub fn svc(&mut self, name: &str, method: &str, args: Vec<Value>) -> Result<Value> {
        let id = self.alloc();
        let frame = json!({"t": "svc", "id": id, "name": name, "method": method, "args": args});
        self.send(&frame)?;
        self.settle((Slot::Rep, id))
    }

    pub fn on_load<F>(&mut self, body: F)
    where
        F: Fn(Value, &mut Next) -> Result<()> + 'static,
    {
        self.load_handler = Some(Rc::new(move |args, next| {
            let config = args.into_iter().next().unwrap_or(Value::Null);
            body(config, next)?;
            Ok(json!("ok"))
        }));
    }

    pub fn on_unload<F>(&mut self, body: F)
    where
        F: Fn(&mut Next) -> Result<()> + 'static,
    {
        self.unload_handler = Some(Rc::new(move |_args, next| {
            body(next)?;
            Ok(Value::Null)
        }));
    }

    pub fn run(mut self) -> ! {
        let hello = json!({"t": "hello", "inject": self.inject});
        if let Err(error) = self.send(&hello) {
            self.log(format!("tenon: hello failed: {error}"));
        }
        loop {
            match self.read() {
                Ok(Some(frame)) => match self.dispatch(frame) {
                    Ok(()) => continue,
                    Err(error) => {
                        if !error.fatal() {
                            self.log(format!("tenon: loop stopped: {error}"));
                        }
                        break;
                    }
                },
                Ok(None) => break,
                Err(error) => {
                    self.log(format!("tenon: read failed: {error}"));
                    break;
                }
            }
        }
        self.shutdown();
        process::exit(0)
    }

    fn alloc(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn register(&mut self, frame: Value) {
        if !self.active {
            self.deferred.push(frame);
            return;
        }
        if let Err(error) = self.send(&frame) {
            self.log(format!("tenon: register failed: {error}"));
        }
    }

    fn send(&mut self, frame: &Value) -> Result<()> {
        let body = serde_json::to_vec(frame)?;
        if body.len() > self.max_frame {
            return Err(Error::FrameTooLarge {
                size: body.len(),
                cap: self.max_frame,
            });
        }
        let mut packet = Vec::with_capacity(4 + body.len());
        packet.extend_from_slice(&(body.len() as u32).to_be_bytes());
        packet.extend_from_slice(&body);
        self.writer.write_all(&packet)?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_exact(&mut self, size: usize) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; size];
        let mut filled = 0;
        while filled < size {
            match self.reader.read(&mut buf[filled..]) {
                Ok(0) => return Ok(None),
                Ok(n) => filled += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Some(buf))
    }

    fn read(&mut self) -> Result<Option<Value>> {
        let Some(head) = self.read_exact(4)? else {
            return Ok(None);
        };
        let size = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let Some(body) = self.read_exact(size)? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(&body)?))
    }

    // Re-entrant by design: waiting for one reply keeps serving inbound frames,
    // so a handler may svc/call and the nested request still completes.
    fn settle(&mut self, slot: (Slot, u64)) -> Result<Value> {
        loop {
            if let Some(reply) = self.replies.remove(&slot) {
                return reply;
            }
            match self.read()? {
                Some(frame) => self.dispatch(frame)?,
                None => return Err(Error::Disconnected),
            }
        }
    }

    fn dispatch(&mut self, frame: Value) -> Result<()> {
        match frame["t"].as_str() {
            Some("hook") => self.on_hook(frame),
            Some("svc") => self.on_svc(frame),
            Some("result") => {
                let req = frame["req"].as_u64().unwrap_or(0);
                self.replies
                    .insert((Slot::Result, req), Ok(frame["result"].clone()));
                Ok(())
            }
            Some("rep") => {
                let id = frame["id"].as_u64().unwrap_or(0);
                self.replies.insert((Slot::Rep, id), reply_value(&frame));
                Ok(())
            }
            Some("load") => self.on_load_frame(frame),
            Some("unload") => Err(Error::Unloaded),
            other => {
                self.log(format!("tenon: ignoring frame {}", other.unwrap_or("?")));
                Ok(())
            }
        }
    }

    fn on_load_frame(&mut self, frame: Value) -> Result<()> {
        let req = frame["req"].as_u64();
        self.config = match frame.get("config") {
            Some(Value::Null) | None => json!({}),
            Some(config) => config.clone(),
        };
        self.active = true;
        for pending in std::mem::take(&mut self.deferred) {
            self.send(&pending)?;
        }
        let config = self.config.clone();
        match self.load_handler.clone() {
            Some(body) => self.guard(req, body, vec![config]),
            None => self.send(&json!({"t": "rep", "req": req, "result": "ok"})),
        }
    }

    fn on_hook(&mut self, frame: Value) -> Result<()> {
        let id = frame["hook"].as_u64().unwrap_or(0);
        let req = frame["req"].as_u64();
        let args = args_of(&frame);
        let entry = self
            .hooks
            .get(&id)
            .map(|hook| (hook.mode, hook.handler.clone()));
        if frame["mode"].as_str() != Some(Mode::Call.as_str()) {
            let Some((_, body)) = entry else {
                return Ok(());
            };
            let mut next = Next {
                plugin: self,
                req: None,
            };
            match body(args, &mut next) {
                Err(error) if error.fatal() => return Err(error),
                Err(error) => self.log(format!("tenon: hook {id} failed: {error}")),
                Ok(_) => {}
            }
            return Ok(());
        }
        match entry {
            Some((_, body)) => self.guard(req, body, args),
            None => self.fail(req, &format!("unknown hook {id}")),
        }
    }

    fn on_svc(&mut self, frame: Value) -> Result<()> {
        let req = frame["req"].as_u64();
        let name = frame["name"].as_str().unwrap_or("");
        let method = frame["method"].as_str().unwrap_or("");
        let impl_ = self
            .services
            .get(name)
            .and_then(|methods| methods.get(method))
            .cloned();
        match impl_ {
            Some(body) => self.guard(req, body, args_of(&frame)),
            None => self.fail(req, &format!("unknown method {method}")),
        }
    }

    fn guard(&mut self, req: Option<u64>, body: Handler, args: Vec<Value>) -> Result<()> {
        let outcome = {
            let mut next = Next { plugin: self, req };
            body(args, &mut next)
        };
        let result = match outcome {
            Ok(result) => result,
            Err(error) if error.fatal() => return Err(error),
            Err(error) => {
                self.log(format!("tenon: request {req:?} failed: {error}"));
                return self.fail(req, &error.to_string());
            }
        };
        match self.send(&json!({"t": "rep", "req": req, "result": result})) {
            Err(Error::FrameTooLarge { size, cap }) => {
                self.log(format!("tenon: reply of {size} bytes over cap {cap}"));
                self.fail(req, "frame_too_large")
            }
            other => other,
        }
    }

    fn fail(&mut self, req: Option<u64>, reason: &str) -> Result<()> {
        if req.is_none() {
            return Ok(());
        }
        let frame = json!({"t": "rep", "req": req, "error": reason});
        match self.send(&frame) {
            Err(Error::FrameTooLarge { .. }) => {
                self.send(&json!({"t": "rep", "req": req, "error": "frame_too_large"}))
            }
            other => other,
        }
    }

    fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        let Some(body) = self.unload_handler.clone() else {
            return;
        };
        let mut next = Next {
            plugin: self,
            req: None,
        };
        if let Err(error) = body(Vec::new(), &mut next) {
            self.log(format!("tenon: unload handler failed: {error}"));
        }
    }
}

pub struct Next<'a> {
    plugin: &'a mut Plugin,
    req: Option<u64>,
}

impl Next<'_> {
    pub fn call(&mut self, args: Vec<Value>) -> Result<Value> {
        let Some(req) = self.req else {
            return Err(Error::msg("next outside a call-mode hook"));
        };
        let frame = json!({"t": "next", "req": req, "args": args, "await": true});
        self.plugin.send(&frame)?;
        self.plugin.settle((Slot::Result, req))
    }

    pub fn svc(&mut self, name: &str, method: &str, args: Vec<Value>) -> Result<Value> {
        self.plugin.svc(name, method, args)
    }

    pub fn waterfall(&mut self, event: &str, args: Vec<Value>) -> Result<Value> {
        self.plugin.call(event, args)
    }

    pub fn emit(&mut self, event: &str, args: Vec<Value>) -> Result<()> {
        self.plugin.emit(event, args)
    }

    pub fn provide(&mut self, name: &str, methods: HashMap<&str, Handler>) {
        self.plugin.provide(name, methods);
    }

    pub fn unprovide(&mut self, name: &str) {
        self.plugin.unprovide(name);
    }

    pub fn on(&mut self, event: &str, mode: Mode, prepend: bool, arity: u64, body: Handler) -> u64 {
        self.plugin.on(event, mode, prepend, arity, body)
    }

    pub fn off(&mut self, hook: u64) {
        self.plugin.off(hook);
    }

    pub fn log(&self, message: impl fmt::Display) {
        self.plugin.log(message);
    }

    pub fn max_frame(&self) -> usize {
        self.plugin.max_frame
    }

    pub fn deadline_ms(&self) -> u64 {
        self.plugin.deadline_ms
    }

    pub fn config(&self) -> &Value {
        &self.plugin.config
    }
}

fn args_of(frame: &Value) -> Vec<Value> {
    frame["args"].as_array().cloned().unwrap_or_default()
}

fn reply_value(frame: &Value) -> Result<Value> {
    match frame.get("error") {
        Some(Value::Null) | None => Ok(frame["result"].clone()),
        Some(error) => Err(Error::Remote(match error.as_str() {
            Some(text) => text.to_string(),
            None => error.to_string(),
        })),
    }
}
