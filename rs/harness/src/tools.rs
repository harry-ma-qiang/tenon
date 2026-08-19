use crate::bus::{Answer, Bus, Gate};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub const MANAGE: &str = "manage";
const RESULT_CHARS: usize = 8_000;

#[derive(Debug, Clone)]
pub struct Row {
    pub name: String,
    pub schema: Value,
    pub service: String,
    pub method: String,
    pub owner: String,
    pub priority: i64,
}

pub struct Outcome {
    pub ok: bool,
    pub denied: bool,
    pub value: Value,
}

impl Outcome {
    pub fn json(&self) -> Value {
        json!({"ok": self.ok, "denied": self.denied, "result": self.value})
    }

    /// The whole result as text, before any cut: what goes to `blobs` when it
    /// is too large for an event row.
    pub fn body(&self) -> String {
        match self.value.as_str() {
            Some(text) => text.to_string(),
            None => self.value.to_string(),
        }
    }

    /// What the model sees as the tool result: the reason on failure, the
    /// value on success, cut to a size a context window survives.
    pub fn text(&self) -> String {
        let body = self.body();
        match body.chars().count() > RESULT_CHARS {
            true => body.chars().take(RESULT_CHARS).collect::<String>() + "\n[truncated]",
            false => body,
        }
    }
}

/// The single authority over model-facing tools (seam 2): one name, one row,
/// and every execution passes the `tools/pre-execute` and `tools/post-execute`
/// waterfalls so a policy plugin anywhere in the node can deny or rewrite it.
pub struct Tools {
    bus: Arc<dyn Bus>,
    rows: Mutex<BTreeMap<String, Row>>,
    seq: Mutex<u64>,
    gated: Mutex<BTreeSet<String>>,
    gate: Mutex<Option<Arc<dyn Gate>>>,
}

impl Tools {
    pub fn new(bus: Arc<dyn Bus>) -> Self {
        Self {
            bus,
            rows: Mutex::new(BTreeMap::new()),
            seq: Mutex::new(0),
            gated: Mutex::new(BTreeSet::new()),
            gate: Mutex::new(None),
        }
    }

    /// The profile's `gated_tools` and the gate that answers for them. An
    /// empty list is the default and costs nothing per call.
    pub fn set_gate(&self, gate: Arc<dyn Gate>, names: &[String]) {
        *self.gate.lock().expect("tools lock") = Some(gate);
        let mut gated = self.gated.lock().expect("tools lock");
        gated.clear();
        for name in names {
            gated.insert(name.clone());
        }
    }

    fn gate_for(&self, name: &str) -> Option<Arc<dyn Gate>> {
        if !self.gated.lock().expect("tools lock").contains(name) {
            return None;
        }
        self.gate.lock().expect("tools lock").clone()
    }

    pub fn register(&self, row: Row) -> Value {
        let mut rows = self.rows.lock().expect("tools lock");
        if let Some(old) = rows.get(&row.name) {
            if old.owner != row.owner && old.priority >= row.priority {
                let reason = format!(
                    "tool {} stays with {} (priority {} >= {})",
                    row.name, old.owner, old.priority, row.priority
                );
                self.bus.log(format!("tenon harness: {reason}"));
                return json!({"ok": false, "kept": old.owner, "reason": reason});
            }
            if old.owner != row.owner {
                self.bus.log(format!(
                    "tenon harness: tool {} taken over by {} (priority {} > {})",
                    row.name, row.owner, row.priority, old.priority
                ));
            }
        }
        let name = row.name.clone();
        rows.insert(name.clone(), row);
        json!({"ok": true, "name": name})
    }

    pub fn unregister(&self, name: &str) -> bool {
        self.rows.lock().expect("tools lock").remove(name).is_some()
    }

    pub fn rows(&self) -> Vec<Row> {
        self.rows
            .lock()
            .expect("tools lock")
            .values()
            .cloned()
            .collect()
    }

    /// The catalog in the shape a chat-completions request wants it.
    pub fn schemas(&self) -> Vec<Value> {
        self.rows()
            .into_iter()
            .map(|row| json!({"type": "function", "function": row.schema}))
            .collect()
    }

    pub fn list(&self) -> Value {
        json!({
            "tools": self
                .rows()
                .into_iter()
                .map(|row| json!({
                    "name": row.name,
                    "owner": row.owner,
                    "priority": row.priority,
                    "target": {"service": row.service, "method": row.method},
                    "schema": row.schema,
                }))
                .collect::<Vec<Value>>(),
        })
    }

    fn call_id(&self) -> String {
        let mut seq = self.seq.lock().expect("tools lock");
        *seq += 1;
        format!("call_{}", *seq)
    }

    pub async fn execute(&self, name: &str, args: Value, call_id: Option<String>) -> Outcome {
        let call_id = call_id.unwrap_or_else(|| self.call_id());
        let Some(row) = self.rows.lock().expect("tools lock").get(name).cloned() else {
            return Outcome {
                ok: false,
                denied: false,
                value: json!(format!("unknown tool {name}")),
            };
        };
        if let Some(gate) = self.gate_for(name) {
            if let Err(reason) = gate.check(name, &args).await {
                return Outcome {
                    ok: false,
                    denied: true,
                    value: json!(reason),
                };
            }
        }
        let request = json!({"name": name, "arguments": args, "callId": call_id});
        let request = match self
            .bus
            .call("tools/pre-execute", vec![request.clone()])
            .await
        {
            Ok(Value::Array(items)) => items.into_iter().next().unwrap_or(request),
            Ok(Value::Object(map)) => {
                let deny = Value::Object(map);
                let reason = deny
                    .get("deny")
                    .and_then(Value::as_str)
                    .or_else(|| deny.get("reason").and_then(Value::as_str))
                    .unwrap_or("denied")
                    .to_string();
                return Outcome {
                    ok: false,
                    denied: true,
                    value: json!(reason),
                };
            }
            Ok(_) | Err(_) => request,
        };
        let arguments = request.get("arguments").cloned().unwrap_or(json!({}));
        let outcome = self
            .bus
            .svc(&row.service, &row.method, vec![arguments])
            .await;
        let (ok, value) = match outcome {
            Ok(value) => (true, value),
            Err(error) => (false, json!(error)),
        };
        let post = json!({"name": name, "callId": call_id, "ok": ok, "result": value});
        let value = match self
            .bus
            .call("tools/post-execute", vec![post.clone()])
            .await
        {
            Ok(Value::Array(items)) => items
                .first()
                .and_then(|item| item.get("result").cloned())
                .unwrap_or(value),
            _ => value,
        };
        Outcome {
            ok,
            denied: false,
            value,
        }
    }

    pub async fn call(&self, method: &str, args: &[Value]) -> Answer {
        let params = args.first().cloned().unwrap_or(Value::Null);
        match method {
            "register" => {
                let name = string(&params, "name");
                if name.is_empty() {
                    return Err("tools.register needs a name".to_string());
                }
                let target = params.get("target").cloned().unwrap_or(Value::Null);
                let service = string(&target, "service");
                let method = string(&target, "method");
                if service.is_empty() || method.is_empty() {
                    return Err("tools.register needs target {service, method}".to_string());
                }
                let mut schema = params.get("schema").cloned().unwrap_or(json!({}));
                if schema.get("name").is_none() {
                    schema["name"] = json!(name);
                }
                Ok(self.register(Row {
                    name,
                    schema,
                    service,
                    method,
                    owner: params
                        .get("owner")
                        .and_then(Value::as_str)
                        .unwrap_or("plugin")
                        .to_string(),
                    priority: params.get("priority").and_then(Value::as_i64).unwrap_or(0),
                }))
            }
            "unregister" => Ok(json!({"ok": self.unregister(&string(&params, "name"))})),
            "list" => Ok(self.list()),
            "execute" => {
                let name = string(&params, "name");
                let args = params.get("args").cloned().unwrap_or(json!({}));
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Ok(self.execute(&name, args, call_id).await.json())
            }
            other => Err(format!("unknown method {other}")),
        }
    }
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "parameters": {
            "type": "object",
            "properties": properties,
            "required": required,
        },
    })
}

fn text(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn number(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

/// The boot catalog: POSIX hands pointed at this env's worker, plus the
/// management tools that let the agent change the environment it runs in.
pub fn builtins(timeout_ms: u64) -> Vec<Row> {
    let worker = |name: &str, method: &str, schema: Value| Row {
        name: name.to_string(),
        schema,
        service: "worker".to_string(),
        method: method.to_string(),
        owner: "harness".to_string(),
        priority: 0,
    };
    let manage = |name: &str, method: &str, schema: Value| Row {
        name: name.to_string(),
        schema,
        service: MANAGE.to_string(),
        method: method.to_string(),
        owner: "harness".to_string(),
        priority: 0,
    };
    vec![
        worker(
            "bash",
            "bash",
            schema(
                "bash",
                &format!("Run a shell command in the workspace. Times out after {timeout_ms} ms."),
                json!({
                    "cmd": text("the command line"),
                    "cwd": text("working directory, relative to the workspace"),
                    "timeout_ms": number("timeout in milliseconds"),
                }),
                &["cmd"],
            ),
        ),
        worker(
            "view_file",
            "fs.view",
            schema(
                "view_file",
                "Read a file, optionally a line range.",
                json!({
                    "path": text("path inside the workspace"),
                    "start": number("first line, 1-based"),
                    "end": number("last line"),
                }),
                &["path"],
            ),
        ),
        worker(
            "write_file",
            "fs.write",
            schema(
                "write_file",
                "Create or overwrite a file with the given content.",
                json!({"path": text("path inside the workspace"), "content": text("file body")}),
                &["path", "content"],
            ),
        ),
        worker(
            "edit_file",
            "fs.edit",
            schema(
                "edit_file",
                "Replace one unique occurrence of old with new in a file.",
                json!({
                    "path": text("path inside the workspace"),
                    "old": text("the exact text to replace, must be unique"),
                    "new": text("the replacement"),
                }),
                &["path", "old", "new"],
            ),
        ),
        worker(
            "grep",
            "fs.grep",
            schema(
                "grep",
                "Search the workspace for a regular expression.",
                json!({"pattern": text("regular expression"), "path": text("subdirectory")}),
                &["pattern"],
            ),
        ),
        worker(
            "glob",
            "fs.glob",
            schema(
                "glob",
                "List workspace paths matching a glob such as **/*.rs.",
                json!({"pattern": text("glob pattern")}),
                &["pattern"],
            ),
        ),
        manage(
            "snapshot",
            "snapshot.tool",
            schema(
                "snapshot",
                "Workspace snapshots: commit the current tree, list them, or restore one.",
                json!({
                    "op": {"type": "string", "enum": ["commit", "list", "restore"]},
                    "label": text("a label for commit"),
                    "ref": text("the snapshot ref or step for restore"),
                }),
                &["op"],
            ),
        ),
        manage(
            "plugin",
            "plugin.tool",
            schema(
                "plugin",
                "Manage the plugins (fibers) of this environment's kernel node.",
                json!({
                    "op": {"type": "string", "enum": ["list", "mount", "unmount", "restart"]},
                    "id": text("the fiber id for unmount and restart"),
                    "spec": {
                        "type": "object",
                        "description": "mount spec: {module} or {cmd, args, env, config, id}",
                    },
                }),
                &["op"],
            ),
        ),
        manage(
            "config",
            "config.tool",
            schema(
                "config",
                "Read or patch this environment's profile overlay. Patches are snapshotted.",
                json!({
                    "op": {"type": "string", "enum": ["get", "patch"]},
                    "patch": {"type": "object", "description": "the overlay patch to merge"},
                }),
                &["op"],
            ),
        ),
        manage(
            "upgrade",
            "upgrade.tool",
            schema(
                "upgrade",
                "Propose a change to this environment through the change protocol: a plugin, \
                 the worker, the kernel or the config. Base snapshots, canaries, verifies \
                 against the benchmark set and then promotes or rolls back with a reason.",
                json!({
                    "op": {"type": "string", "enum": ["propose", "status", "list"]},
                    "target": {"type": "string", "enum": ["plugin", "worker", "kernel", "config"]},
                    "artifact": {
                        "type": "object",
                        "description": "plugin: {name, version, spec:{cmd,args,env}, service, \
                                        selfcheck}; worker: {cmd, args, env}; kernel: {beam}; \
                                        config: {patch}",
                    },
                    "notes": text("why this change"),
                    "id": number("the proposal id, for status"),
                }),
                &["op"],
            ),
        ),
        manage(
            "runtime_spawn",
            "runtime.spawn",
            schema(
                "runtime_spawn",
                "Ask the barebone for a child environment of this one.",
                json!({"overrides": {"type": "object", "description": "name, ram_mb, patch"}}),
                &[],
            ),
        ),
        manage(
            "approval_request",
            "approval.request",
            schema(
                "approval_request",
                "Ask a human to approve something that would affect the host.",
                json!({"reason": text("what you want to do and why")}),
                &["reason"],
            ),
        ),
    ]
}
