use crate::bus::{Answer, BoxFut, Bus};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::fd::FromRawFd;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tenon_base::frame;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

const WIRE_IN_FD: i32 = 3;
const WIRE_OUT_FD: i32 = 4;
const DEFAULT_MAX_FRAME: usize = 1_048_576;

pub type Method = Arc<dyn Fn(Vec<Value>) -> BoxFut<'static, Answer> + Send + Sync>;
pub type Reader = Pin<Box<dyn AsyncRead + Send + Unpin>>;
pub type Writer = Pin<Box<dyn AsyncWrite + Send + Unpin>>;

#[derive(Default)]
pub struct Router {
    services: HashMap<String, HashMap<String, Method>>,
}

impl Router {
    pub fn service(&mut self, name: &str, methods: Vec<(&str, Method)>) {
        let entry = self.services.entry(name.to_string()).or_default();
        for (method, body) in methods {
            entry.insert(method.to_string(), body);
        }
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.services.keys().cloned().collect();
        names.sort();
        names
    }

    fn lookup(&self, service: &str, method: &str) -> Option<Method> {
        self.services.get(service)?.get(method).cloned()
    }
}

pub struct Wire {
    out: mpsc::UnboundedSender<Value>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
    seq: AtomicU64,
    max_frame: usize,
}

impl Wire {
    fn new(out: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            out,
            pending: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            max_frame: max_frame(),
        }
    }

    pub fn send(&self, frame: Value) {
        let _ = self.out.send(frame);
    }

    async fn request(&self, mut body: Value) -> Answer {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        if let Some(target) = body.as_object_mut() {
            target.insert("id".to_string(), json!(id));
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("wire lock").insert(id, tx);
        if self.out.send(body).is_err() {
            self.pending.lock().expect("wire lock").remove(&id);
            return Err("wire closed".to_string());
        }
        rx.await.unwrap_or_else(|_| Err("wire closed".to_string()))
    }

    fn resolve(&self, frame: &Value) {
        let Some(id) = frame.get("id").and_then(Value::as_u64) else {
            return;
        };
        let waiter = self.pending.lock().expect("wire lock").remove(&id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(frame::outcome(frame));
        }
    }

    fn fail_all(&self) {
        let waiting: Vec<_> = self
            .pending
            .lock()
            .expect("wire lock")
            .drain()
            .map(|(_, waiter)| waiter)
            .collect();
        for waiter in waiting {
            let _ = waiter.send(Err("wire closed".to_string()));
        }
    }
}

impl Bus for Wire {
    fn svc<'a>(&'a self, name: &str, method: &str, args: Vec<Value>) -> BoxFut<'a, Answer> {
        let body = json!({"t": "svc", "name": name, "method": method, "args": args});
        Box::pin(async move { self.request(body).await })
    }

    fn call<'a>(&'a self, event: &str, args: Vec<Value>) -> BoxFut<'a, Answer> {
        let body = json!({"t": "call", "event": event, "args": args});
        Box::pin(async move { self.request(body).await })
    }

    fn emit(&self, event: &str, args: Vec<Value>) {
        self.send(json!({"t": "emit", "event": event, "args": args}));
    }

    fn max_frame(&self) -> usize {
        self.max_frame
    }
}

fn max_frame() -> usize {
    std::env::var("TENON_MAX_FRAME")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_FRAME)
}

/// The two wire ends: a connection to `TENON_GATEWAY` when base handed us one,
/// fd 3/4 when the kernel spawned us directly. Both are framed the same way.
pub async fn ends(address: Option<&str>) -> anyhow::Result<(Reader, Writer)> {
    match address.map(str::trim).filter(|text| !text.is_empty()) {
        Some(address) => connect(address).await,
        None => {
            let read = unsafe { std::fs::File::from_raw_fd(WIRE_IN_FD) };
            let write = unsafe { std::fs::File::from_raw_fd(WIRE_OUT_FD) };
            Ok((
                Box::pin(tokio::fs::File::from_std(read)),
                Box::pin(tokio::fs::File::from_std(write)),
            ))
        }
    }
}

async fn connect(address: &str) -> anyhow::Result<(Reader, Writer)> {
    if let Some(path) = address.strip_prefix("unix:") {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (read, write) = stream.into_split();
        return Ok((Box::pin(read), Box::pin(write)));
    }
    if let Some(rest) = address.strip_prefix("tcp:") {
        let stream = tokio::net::TcpStream::connect(rest).await?;
        let (read, write) = stream.into_split();
        return Ok((Box::pin(read), Box::pin(write)));
    }
    anyhow::bail!("bad TENON_GATEWAY address: {address}")
}

/// Opens the writing half: every frame the harness sends goes through one
/// channel, so any task may send without owning the socket.
pub fn open(writer: Writer) -> Arc<Wire> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    let wire = Arc::new(Wire::new(tx));
    let mut writer = writer;
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if frame::write(&mut writer, &frame).await.is_err() {
                break;
            }
        }
    });
    wire
}

/// Runs the plugin protocol until the wire closes or an `unload` arrives.
/// `ready` is handed the loaded config once the kernel has accepted the
/// services; every inbound `svc` is answered from its own task, so a tool call
/// that takes a minute never blocks the next frame.
pub async fn serve<F>(wire: Arc<Wire>, reader: Reader, router: Router, ready: F)
where
    F: FnOnce(Arc<Wire>, Value) + Send + 'static,
{
    wire.send(json!({"t": "hello", "inject": []}));
    let router = Arc::new(router);
    let mut reader = reader;
    let mut ready = Some(ready);
    while let Ok(Some(body)) = frame::read(&mut reader).await {
        match frame::method(&body) {
            Some("rep") => wire.resolve(&body),
            Some("load") => {
                let config = body.get("config").cloned().unwrap_or(Value::Null);
                for name in router.names() {
                    wire.send(json!({"t": "provide", "name": name}));
                }
                wire.send(json!({"t": "rep", "req": body.get("req"), "result": "ok"}));
                if let Some(ready) = ready.take() {
                    ready(wire.clone(), config);
                }
            }
            Some("svc") => dispatch(&wire, &router, body),
            Some("unload") => break,
            _ => {}
        }
    }
    wire.fail_all();
}

fn dispatch(wire: &Arc<Wire>, router: &Arc<Router>, body: Value) {
    let req = body.get("req").cloned().unwrap_or(Value::Null);
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let method = body
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = body
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(handler) = router.lookup(&name, &method) else {
        wire.send(
            json!({"t": "rep", "req": req, "error": format!("unknown method {name}.{method}")}),
        );
        return;
    };
    let wire = wire.clone();
    tokio::spawn(async move {
        let frame = match handler(args).await {
            Ok(result) => json!({"t": "rep", "req": req, "result": result}),
            Err(error) => json!({"t": "rep", "req": req, "error": error}),
        };
        wire.send(frame);
    });
}

pub fn method<F, R>(body: F) -> Method
where
    F: Fn(Vec<Value>) -> R + Send + Sync + 'static,
    R: std::future::Future<Output = Answer> + Send + 'static,
{
    Arc::new(move |args| Box::pin(body(args)))
}
