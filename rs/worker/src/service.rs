use crate::fs::Fs;
use crate::pty::{bash, BashReq, Ptys};
use crate::snap::Snap;
use crate::{err, out_dir, Result, DEFAULT_SERVICE, TAIL_BYTES};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use tenon_sdk::{arg, handler, Handler, Next, Plugin};

const MIN_CAP: usize = 8_192;
const MAX_CAP: usize = 262_144;
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_READ: usize = 65_536;
const KEEP_LAST: usize = 20;
const MILESTONE_EVERY: usize = 50;

pub struct Worker {
    root: PathBuf,
    fs: Fs,
    snap: Snap,
    ptys: Ptys,
    step: Cell<u64>,
    seq: Cell<u64>,
    cap: Cell<usize>,
}

type Shared = Rc<Worker>;

impl Worker {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            fs: Fs::new(root),
            snap: Snap::at(root),
            ptys: Ptys::new(root),
            step: Cell::new(0),
            seq: Cell::new(0),
            cap: Cell::new(MAX_CAP),
        })
    }

    fn handle(&self, kind: &str, suffix: &str) -> Result<PathBuf> {
        self.seq.set(self.seq.get() + 1);
        Ok(out_dir(&self.root)?.join(format!("{kind}-{}.{suffix}", self.seq.get())))
    }

    fn under(&self, path: Option<&str>) -> Result<PathBuf> {
        let Some(path) = path.filter(|text| !text.is_empty()) else {
            return Ok(self.root.clone());
        };
        let joined = match Path::new(path).is_absolute() {
            true => PathBuf::from(path),
            false => self.root.join(path),
        };
        let clean = normalize(&joined);
        if !clean.starts_with(&self.root) {
            return Err(err(format!("{path} is outside the workspace")));
        }
        Ok(clean)
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn serve(root: &Path) -> Result<()> {
    let state: Shared = Rc::new(Worker::new(root)?);
    let mut plugin = Plugin::try_new(&[])?;

    let loaded = state.clone();
    plugin.on_load(move |config: Value, next: &mut Next| {
        let name = config["service"]
            .as_str()
            .unwrap_or(DEFAULT_SERVICE)
            .to_string();
        loaded
            .cap
            .set((next.max_frame() / 8).clamp(MIN_CAP, MAX_CAP));
        next.provide(&name, methods(loaded.clone()));
        next.log(format!(
            "tenon worker: service {name}, workspace {}, cap {}",
            loaded.root.display(),
            loaded.cap.get()
        ));
        Ok(())
    });

    let unloaded = state.clone();
    plugin.on_unload(move |next: &mut Next| {
        unloaded.ptys.close_all();
        next.log("tenon worker: unloaded, sessions closed");
        Ok(())
    });

    plugin.run()
}

fn methods(state: Shared) -> HashMap<&'static str, Handler> {
    let mut methods: HashMap<&'static str, Handler> = HashMap::new();
    for (name, body) in [
        ("ping", 0usize),
        ("info", 1),
        ("bash", 2),
        ("pty.open", 3),
        ("pty.send", 4),
        ("pty.read", 5),
        ("pty.close", 6),
        ("fs.view", 7),
        ("fs.write", 8),
        ("fs.edit", 9),
        ("fs.grep", 10),
        ("fs.glob", 11),
        ("snap.commit", 12),
        ("snap.list", 13),
        ("snap.restore", 14),
        ("snap.diff", 15),
        ("snap.pack", 16),
        ("snap.apply", 17),
        ("snap.expire", 18),
    ] {
        let shared = state.clone();
        methods.insert(
            name,
            handler(move |args: Vec<Value>, next: &mut Next| {
                dispatch(&shared, body, name, args, next)
            }),
        );
    }
    methods
}

fn dispatch(
    state: &Shared,
    slot: usize,
    name: &'static str,
    args: Vec<Value>,
    next: &mut Next,
) -> Result<Value> {
    let params = arg(&args, 0).clone();
    let result = call(state, slot, &params, next)?;
    if mutating(name) {
        state.step.set(state.step.get() + 1);
        let step = json!({
            "step": state.step.get(),
            "method": name,
            "ref": result.get("ref").cloned().unwrap_or(Value::Null),
        });
        next.emit("worker/step", vec![step])?;
    }
    cap(state, name, result)
}

fn mutating(name: &str) -> bool {
    matches!(
        name,
        "bash"
            | "fs.write"
            | "fs.edit"
            | "pty.open"
            | "pty.send"
            | "pty.close"
            | "snap.commit"
            | "snap.restore"
            | "snap.apply"
            | "snap.expire"
    )
}

fn call(state: &Shared, slot: usize, p: &Value, _next: &mut Next) -> Result<Value> {
    match slot {
        0 => Ok(json!("pong")),
        1 => Ok(json!({
            "pid": std::process::id(),
            "workspace": state.root,
            "step": state.step.get(),
            "sessions": state.ptys.count(),
            "cap": state.cap.get(),
        })),
        2 => run_bash(state, p),
        3 => {
            let cwd = state.under(text(p, "cwd").as_deref())?;
            state.ptys.open(
                text(p, "cmd").as_deref(),
                cwd.to_str(),
                &environment(p),
                number(p, "cols").unwrap_or(0) as u16,
                number(p, "rows").unwrap_or(0) as u16,
            )
        }
        4 => state.ptys.send(
            session(p)?,
            text(p, "data")
                .ok_or_else(|| err("pty.send needs data"))?
                .as_str(),
        ),
        5 => state.ptys.read(
            session(p)?,
            number(p, "max").unwrap_or(DEFAULT_READ as u64) as usize,
        ),
        6 => state.ptys.close(session(p)?),
        7 => state.fs.view(
            &required(p, "path")?,
            number(p, "start").map(|n| n as usize),
            number(p, "end").map(|n| n as usize),
        ),
        8 => state.fs.write(
            &required(p, "path")?,
            text(p, "content").unwrap_or_default().as_str(),
        ),
        9 => state.fs.edit(
            &required(p, "path")?,
            &required(p, "old")?,
            text(p, "new").unwrap_or_default().as_str(),
        ),
        10 => state
            .fs
            .grep(&required(p, "pattern")?, text(p, "path").as_deref()),
        11 => state.fs.glob(&required(p, "pattern")?),
        12 => state.snap.commit(text(p, "label").as_deref()),
        13 => state.snap.list(),
        14 => state.snap.restore(&reference(p, "ref")?),
        15 => state.snap.diff(&reference(p, "a")?, &reference(p, "b")?),
        16 => pack(state, p),
        17 => apply(state, p),
        18 => state.snap.expire(
            number(p, "keep_last").unwrap_or(KEEP_LAST as u64) as usize,
            number(p, "milestone_every").unwrap_or(MILESTONE_EVERY as u64) as usize,
        ),
        _ => Err(err("unknown method")),
    }
}

fn run_bash(state: &Shared, p: &Value) -> Result<Value> {
    let cmd = required(p, "cmd")?;
    let outcome = bash(&BashReq {
        cmd,
        cwd: state.under(text(p, "cwd").as_deref())?,
        timeout_ms: number(p, "timeout_ms").unwrap_or(DEFAULT_TIMEOUT_MS),
        env: environment(p),
        pty: p.get("pty").and_then(Value::as_bool).unwrap_or(true),
        spill_dir: out_dir(&state.root)?,
        tail_bytes: TAIL_BYTES,
    })?;
    Ok(json!({
        "status": outcome.status,
        "timed_out": outcome.timed_out,
        "bytes": outcome.bytes,
        "tail": outcome.tail,
        "handle": outcome.spill.map(|path| path.display().to_string()),
        "truncated": outcome.bytes > TAIL_BYTES,
    }))
}

fn pack(state: &Shared, p: &Value) -> Result<Value> {
    let built = state.snap.pack(number(p, "since"))?;
    let mut answer = json!({
        "step": built.step,
        "ref": built.head,
        "bytes": built.bytes.len(),
        "refs": built.refs.iter().map(|(step, oid)| json!({"step": step, "ref": oid}))
            .collect::<Vec<Value>>(),
    });
    let encoded = base64(&built.bytes);
    let row = answer.as_object_mut().ok_or_else(|| err("pack answer"))?;
    if encoded.len() > state.cap.get() {
        let path = state.handle("pack", "pack")?;
        std::fs::write(&path, &built.bytes)?;
        row.insert("handle".to_string(), json!(path.display().to_string()));
    } else {
        row.insert("pack".to_string(), json!(encoded));
    }
    Ok(answer)
}

fn apply(state: &Shared, p: &Value) -> Result<Value> {
    let rows = p
        .get("packs")
        .and_then(Value::as_array)
        .ok_or_else(|| err("snap.apply needs packs"))?;
    let mut packs = Vec::new();
    for row in rows {
        let step = row.get("step").and_then(Value::as_u64).unwrap_or(0);
        let oid = row
            .get("ref")
            .and_then(Value::as_str)
            .ok_or_else(|| err("every pack row needs a ref"))?
            .to_string();
        let path = match row.get("handle").and_then(Value::as_str) {
            Some(path) => state.under(Some(path))?,
            None => {
                let bytes = decode(
                    row.get("pack")
                        .and_then(Value::as_str)
                        .ok_or_else(|| err("every pack row needs pack or handle"))?,
                )?;
                let path = state.handle("apply", "pack")?;
                std::fs::write(&path, &bytes)?;
                path
            }
        };
        packs.push((step, oid, path));
    }
    packs.sort_by_key(|(step, _, _)| *step);
    state.snap.apply(&packs, text(p, "ref").as_deref())
}

fn cap(state: &Shared, name: &str, value: Value) -> Result<Value> {
    let body = value.to_string();
    if body.len() <= state.cap.get() {
        return Ok(value);
    }
    let path = state.handle(&name.replace('.', "-"), "json")?;
    std::fs::write(&path, &body)?;
    Ok(json!({
        "handle": path.display().to_string(),
        "bytes": body.len(),
        "over_cap": true,
        "method": name,
    }))
}

fn session(p: &Value) -> Result<u64> {
    p.get("session")
        .and_then(Value::as_u64)
        .ok_or_else(|| err("this method needs a session id"))
}

fn required(p: &Value, key: &str) -> Result<String> {
    text(p, key).ok_or_else(|| err(format!("missing {key}")))
}

fn reference(p: &Value, key: &str) -> Result<String> {
    match p.get(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Number(number)) => Ok(number.to_string()),
        _ => Err(err(format!("missing {key}"))),
    }
}

fn text(p: &Value, key: &str) -> Option<String> {
    p.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.is_empty())
}

fn number(p: &Value, key: &str) -> Option<u64> {
    p.get(key).and_then(Value::as_u64)
}

fn environment(p: &Value) -> Vec<(String, String)> {
    match p.get("env").and_then(Value::as_object) {
        None => vec![],
        Some(rows) => rows
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                (name.clone(), value)
            })
            .collect(),
    }
}

fn base64(body: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(body)
}

fn decode(body: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|error| err(format!("bad base64 pack: {error}")))
}
