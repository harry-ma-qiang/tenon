use crate::bus::{Answer, BoxFut, McpCall};
use crate::tools::{Row, Tools};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

const PROTOCOL: &str = "2024-11-05";

/// One connection to an external MCP server (RFC P4.7 client side): a
/// newline-delimited JSON-RPC 2.0 peer, spawned over stdio or reached over HTTP.
/// Its `tools/list` tools are registered into our tools bus under
/// `mcp/<name>/<tool>` and its `tools/call` is forwarded through this handle, so
/// bridged tools obey the same single authority as native ones.
struct Server {
    kind: Kind,
    seq: AtomicI64,
    tools: Vec<String>,
}

enum Kind {
    Stdio {
        out: mpsc::UnboundedSender<Value>,
        pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
        child: Mutex<Option<tokio::process::Child>>,
    },
    Http {
        url: String,
        client: reqwest::Client,
    },
}

/// Every mounted MCP server, keyed by name. The manager is the tools bus's
/// `McpCall` target: a bridged tool call arrives here already past the
/// pre-execute waterfall and the gate.
pub struct McpManager {
    servers: Mutex<HashMap<String, Arc<Server>>>,
}

impl McpManager {
    pub fn new() -> Arc<McpManager> {
        Arc::new(McpManager {
            servers: Mutex::new(HashMap::new()),
        })
    }

    pub async fn call_method(&self, method: &str, params: &Value, tools: &Arc<Tools>) -> Answer {
        match method {
            "mount" => self.mount(params, tools).await,
            "list" => Ok(self.list()),
            "unmount" => self.unmount(params, tools),
            other => Err(format!("unknown method {other}")),
        }
    }

    async fn mount(&self, params: &Value, tools: &Arc<Tools>) -> Answer {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or("mcp.mount needs a name")?
            .to_string();
        if self.servers.lock().expect("mcp").contains_key(&name) {
            return Err(format!("mcp server {name} already mounted"));
        }
        let server = match params.get("url").and_then(Value::as_str) {
            Some(url) => Arc::new(Server {
                kind: Kind::Http {
                    url: url.to_string(),
                    client: reqwest::Client::new(),
                },
                seq: AtomicI64::new(0),
                tools: Vec::new(),
            }),
            None => spawn_stdio(params).await?,
        };
        server
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL,
                    "capabilities": {},
                    "clientInfo": {"name": "tenon", "version": "0.1.0"},
                }),
            )
            .await?;
        server.notify("notifications/initialized", json!({}));
        let listed = server.request("tools/list", json!({})).await?;
        let mut registered = Vec::new();
        let mut server = server;
        for tool in listed
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(tool_name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let qualified = format!("{name}/{tool_name}");
            let row_name = format!("mcp/{qualified}");
            let schema = mcp_schema(&row_name, tool);
            tools.register(Row {
                name: row_name.clone(),
                schema,
                service: "mcp".to_string(),
                method: qualified,
                owner: format!("mcp/{name}"),
                priority: 0,
            });
            registered.push(row_name);
        }
        if let Some(inner) = Arc::get_mut(&mut server) {
            inner.tools = registered.clone();
        }
        self.servers
            .lock()
            .expect("mcp")
            .insert(name.clone(), server);
        Ok(json!({"ok": true, "name": name, "tools": registered}))
    }

    fn unmount(&self, params: &Value, tools: &Arc<Tools>) -> Answer {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(server) = self.servers.lock().expect("mcp").remove(name) else {
            return Ok(json!({"ok": false, "name": name}));
        };
        for row in &server.tools {
            tools.unregister(row);
        }
        server.shutdown();
        Ok(json!({"ok": true, "name": name}))
    }

    fn list(&self) -> Value {
        let servers = self.servers.lock().expect("mcp");
        let rows: Vec<Value> = servers
            .iter()
            .map(|(name, server)| json!({"name": name, "tools": server.tools}))
            .collect();
        json!({"count": rows.len(), "servers": rows})
    }
}

impl McpCall for McpManager {
    fn call<'a>(&'a self, qualified: &str, args: Value) -> BoxFut<'a, Answer> {
        let qualified = qualified.to_string();
        Box::pin(async move {
            let (server_name, tool) = qualified
                .split_once('/')
                .ok_or_else(|| format!("bad mcp tool {qualified}"))?;
            let server = self
                .servers
                .lock()
                .expect("mcp")
                .get(server_name)
                .cloned()
                .ok_or_else(|| format!("mcp server {server_name} not mounted"))?;
            let result = server
                .request("tools/call", json!({"name": tool, "arguments": args}))
                .await?;
            content(&result)
        })
    }
}

impl Server {
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match &self.kind {
            Kind::Stdio { out, pending, .. } => {
                let (tx, rx) = oneshot::channel();
                pending.lock().expect("mcp pending").insert(id, tx);
                out.send(frame).map_err(|_| "mcp server gone".to_string())?;
                let reply = rx.await.map_err(|_| "mcp server closed".to_string())?;
                outcome(&reply)
            }
            Kind::Http { url, client } => {
                let response = client
                    .post(url)
                    .json(&frame)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                let reply: Value = response.json().await.map_err(|error| error.to_string())?;
                outcome(&reply)
            }
        }
    }

    fn notify(&self, method: &str, params: Value) {
        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Kind::Stdio { out, .. } = &self.kind {
            let _ = out.send(frame);
        }
    }

    fn shutdown(&self) {
        if let Kind::Stdio { child, .. } = &self.kind {
            if let Some(mut child) = child.lock().expect("mcp child").take() {
                let _ = child.start_kill();
            }
        }
    }
}

async fn spawn_stdio(params: &Value) -> Result<Arc<Server>, String> {
    let cmd = params
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or("mcp.mount needs cmd or url")?;
    let args: Vec<String> = params
        .get("args")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut command = tokio::process::Command::new(cmd);
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(env) = params.get("env").and_then(Value::as_object) {
        for (name, value) in env {
            if let Some(value) = value.as_str() {
                command.env(name, value);
            }
        }
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let stdin = child.stdin.take().ok_or("mcp stdin")?;
    let stdout = child.stdout.take().ok_or("mcp stdout")?;
    let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let mut writer = stdin;
    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let mut line = frame.to_string();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = writer.flush().await;
        }
    });
    let reader_pending = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(id) = frame.get("id").and_then(Value::as_i64) {
                if let Some(tx) = reader_pending.lock().expect("mcp pending").remove(&id) {
                    let _ = tx.send(frame);
                }
            }
        }
    });
    Ok(Arc::new(Server {
        kind: Kind::Stdio {
            out: out_tx,
            pending,
            child: Mutex::new(Some(child)),
        },
        seq: AtomicI64::new(0),
        tools: Vec::new(),
    }))
}

fn outcome(reply: &Value) -> Result<Value, String> {
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("mcp error");
        return Err(message.to_string());
    }
    Ok(reply.get("result").cloned().unwrap_or(Value::Null))
}

/// An MCP `tools/call` result to the tools bus's flat value: the joined text of
/// its `content` blocks; an `isError` result is a tool error the model reads.
fn content(result: &Value) -> Result<Value, String> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<&str>>()
                .join("\n")
        })
        .unwrap_or_else(|| result.to_string());
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(text);
    }
    Ok(Value::String(text))
}

fn mcp_schema(name: &str, tool: &Value) -> Value {
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("bridged MCP tool");
    let parameters = tool
        .get("inputSchema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    json!({"name": name, "description": description, "parameters": parameters})
}
