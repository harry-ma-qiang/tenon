use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tenon_sdk::{arg, handler, Handler, Mode, Next, Plugin, Result};

struct State {
    name: String,
    peer: Option<String>,
    audits: i64,
}

fn main() {
    let state = Rc::new(RefCell::new(State {
        name: "demo".to_string(),
        peer: None,
        audits: 0,
    }));
    let mut plugin = Plugin::new(&[]);

    let loaded = state.clone();
    plugin.on_load(move |config: Value, next: &mut Next| {
        let name = config["service"].as_str().unwrap_or("demo").to_string();
        {
            let mut state = loaded.borrow_mut();
            state.name = name.clone();
            state.peer = config["peer"].as_str().map(str::to_string);
        }
        next.provide(&name, methods(loaded.clone()));
        next.log(format!("demo plugin loaded as {name}"));
        Ok(())
    });

    let unloaded = state.clone();
    plugin.on_unload(move |next: &mut Next| {
        next.log(format!("demo plugin {} unloading", unloaded.borrow().name));
        Ok(())
    });

    let guarded = state.clone();
    plugin.on(
        "tools/execute",
        Mode::Call,
        true,
        1,
        handler(move |args: Vec<Value>, next: &mut Next| guard(&guarded, args, next)),
    );

    let audited = state.clone();
    plugin.on(
        "sys/audit",
        Mode::Emit,
        false,
        1,
        handler(move |_args: Vec<Value>, _next: &mut Next| {
            audited.borrow_mut().audits += 1;
            Ok(Value::Null)
        }),
    );

    plugin.run()
}

fn methods(state: Rc<RefCell<State>>) -> HashMap<&'static str, Handler> {
    let counter = state;
    let mut methods: HashMap<&'static str, Handler> = HashMap::new();
    methods.insert(
        "ping",
        handler(|_args: Vec<Value>, _next: &mut Next| Ok(json!("pong"))),
    );
    methods.insert(
        "add",
        handler(|args: Vec<Value>, _next: &mut Next| Ok(sum(arg(&args, 0), arg(&args, 1)))),
    );
    methods.insert(
        "getenv",
        handler(|args: Vec<Value>, _next: &mut Next| {
            let name = arg(&args, 0).as_str().unwrap_or("").to_string();
            Ok(json!(std::env::var(name).unwrap_or_default()))
        }),
    );
    methods.insert(
        "count",
        handler(move |_args: Vec<Value>, _next: &mut Next| Ok(json!(counter.borrow().audits))),
    );
    methods.insert(
        "big",
        handler(|args: Vec<Value>, _next: &mut Next| {
            let size = arg(&args, 0).as_u64().unwrap_or(0) as usize;
            Ok(json!("x".repeat(size)))
        }),
    );
    methods.insert(
        "pid",
        handler(|_args: Vec<Value>, _next: &mut Next| Ok(json!(std::process::id()))),
    );
    methods
}

fn guard(state: &Rc<RefCell<State>>, args: Vec<Value>, next: &mut Next) -> Result<Value> {
    let (name, peer) = {
        let state = state.borrow();
        (state.name.clone(), state.peer.clone())
    };
    let request = arg(&args, 0).clone();
    let command = command_of(&request);
    if command.contains("rm -rf") {
        return Ok(json!({"status": "blocked", "by": name, "cmd": command}));
    }
    let mut entry = Map::new();
    entry.insert("by".to_string(), json!(name));
    if let Some(peer) = peer {
        entry.insert("peer".to_string(), next.svc(&peer, "ping", vec![])?);
    }
    let mut seen = request["seen"].as_array().cloned().unwrap_or_default();
    seen.push(Value::Object(entry));
    let mut forwarded = request.as_object().cloned().unwrap_or_default();
    forwarded.insert("seen".to_string(), Value::Array(seen));
    let result = next.call(vec![Value::Object(forwarded)])?;
    Ok(json!({"guarded": name, "result": result}))
}

fn command_of(request: &Value) -> String {
    match request.get("cmd") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None if request.is_object() => String::new(),
        None => request.to_string(),
    }
}

fn sum(left: &Value, right: &Value) -> Value {
    match (left.as_i64(), right.as_i64()) {
        (Some(a), Some(b)) => json!(a + b),
        _ => json!(left.as_f64().unwrap_or(0.0) + right.as_f64().unwrap_or(0.0)),
    }
}
