use crate::client::Client;
use crate::home::Home;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL: &str = "2024-11-05";

/// Tenon as an MCP server (RFC P4.7 server side): it exposes this env's tools
/// bus over JSON-RPC 2.0 so an MCP client (Claude Code, another agent) can run
/// bash and edit files inside the sandbox. `tools/call` is routed through the
/// harness tools bus, so it is env-scoped and gated tools go through approvals
/// exactly as a model-issued call. Two transports carry it: stdio (`tenon mcp`)
/// and streamable HTTP on serve (`/mcp`, token-authorized in `http.rs`).
///
/// `tenon mcp [--env NAME]`: newline-delimited JSON-RPC over stdin/stdout.
pub async fn stdio(home: Option<PathBuf>, env: Option<String>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let config = crate::config::Config::load(&home.config_file()).ok();
    let env = env
        .or_else(|| config.map(|config| config.root_env))
        .unwrap_or_else(|| "root".to_string());
    let sock = home.sock();
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(response) = handle(&request, &env, &sock).await {
            let mut body = response.to_string();
            body.push('\n');
            stdout.write_all(body.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(0)
}

/// One JSON-RPC 2.0 message in, at most one out (a request without an `id` is a
/// notification and gets no reply). This is the shared core of both transports.
pub async fn handle(request: &Value, env: &str, sock: &Path) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(json!({}));
    if method.starts_with("notifications/") {
        return None;
    }
    let outcome = dispatch(method, &params, env, sock).await;
    let id = id?;
    Some(match outcome {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    })
}

async fn dispatch(
    method: &str,
    params: &Value,
    env: &str,
    sock: &Path,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "tenon", "version": "0.1.0"},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": list_tools(env, sock).await})),
        "tools/call" => call_tool(params, env, sock).await,
        other => Err((-32601, format!("method not found: {other}"))),
    }
}

/// The env's tools bus catalog, mapped to MCP tool descriptors. If the harness
/// is not up the list is empty rather than an error — the server still answers.
async fn list_tools(env: &str, sock: &Path) -> Vec<Value> {
    let Ok(mut client) = Client::connect(sock).await else {
        return Vec::new();
    };
    let answer = client
        .call(
            "svc",
            json!({"env": env, "name": "tools", "method": "list", "args": [{}]}),
        )
        .await;
    let Ok(answer) = answer else {
        return Vec::new();
    };
    answer
        .get("tools")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(descriptor).collect())
        .unwrap_or_default()
}

fn descriptor(row: &Value) -> Option<Value> {
    let name = row.get("name").and_then(Value::as_str)?;
    let schema = row.get("schema");
    let description = schema
        .and_then(|schema| schema.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("tenon tool");
    let input = schema
        .and_then(|schema| schema.get("parameters"))
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    Some(json!({"name": name, "description": description, "inputSchema": input}))
}

async fn call_tool(params: &Value, env: &str, sock: &Path) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call needs a name".to_string()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let mut client = Client::connect(sock)
        .await
        .map_err(|error| (-32603, error.to_string()))?;
    let answer = client
        .call(
            "svc",
            json!({
                "env": env,
                "name": "tools",
                "method": "execute",
                "args": [{"name": name, "args": arguments}],
            }),
        )
        .await
        .map_err(|error| (-32603, error.to_string()))?;
    let ok = answer.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let denied = answer
        .get("denied")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let result = answer.get("result").cloned().unwrap_or(Value::Null);
    let text = match result.as_str() {
        Some(text) => text.to_string(),
        None => result.to_string(),
    };
    Ok(json!({
        "content": [{"type": "text", "text": text}],
        "isError": !ok || denied,
    }))
}
