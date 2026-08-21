use crate::home::Home;
use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const MAX_REQUEST: usize = 256 * 1024;

/// Tenon-as-MCP over a loopback TCP port, bearer-token authorized, for a jailed
/// cli-agent (RFC section 3 layer B). It exists because the jail deliberately
/// does not grant the front-door unix socket under `~/.tenon/run`, so the only
/// Tenon surface the agent can reach is a loopback port. Each `POST /mcp` is one
/// JSON-RPC message routed through `crate::mcp::handle` into that env's tools bus
/// (env-scoped, gated, snapshotted like any model-issued call). Requires a
/// running base — the tool calls hop the front-door socket. Bound to `:0`, so
/// the OS picks a free port the caller reads back.
pub struct McpServer {
    pub url: String,
    pub token: String,
    handle: JoinHandle<()>,
}

impl McpServer {
    pub fn stop(self) {
        self.handle.abort();
    }
}

pub async fn start(home: &Home, env: &str, token: String) -> Result<McpServer> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let sock = home.sock();
    let env = env.to_string();
    let guard = token.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let sock = sock.clone();
            let env = env.clone();
            let guard = guard.clone();
            tokio::spawn(async move {
                let _ = serve_conn(stream, sock, env, guard).await;
            });
        }
    });
    Ok(McpServer {
        url: format!("http://127.0.0.1:{port}/mcp"),
        token,
        handle,
    })
}

async fn serve_conn(
    mut stream: TcpStream,
    sock: std::path::PathBuf,
    env: String,
    token: String,
) -> Result<()> {
    let Some((head, body)) = read_request(&mut stream).await? else {
        return Ok(());
    };
    if bearer(&head).as_deref() != Some(token.as_str()) {
        return reply(&mut stream, 401, "text/plain", "unauthorized").await;
    }
    let request = serde_json::from_str::<serde_json::Value>(&body).unwrap_or_default();
    match crate::mcp::handle(&request, &env, &sock).await {
        Some(response) => reply(&mut stream, 200, "application/json", &response.to_string()).await,
        None => reply(&mut stream, 202, "text/plain", "").await,
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<Option<(String, String)>> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let (head, mut body) = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(None);
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.len() > MAX_REQUEST {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&raw).to_string();
        if let Some(cut) = text.find("\r\n\r\n") {
            break (text[..cut].to_string(), raw[cut + 4..].to_vec());
        }
    };
    let want = content_length(&head);
    while body.len() < want {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Ok(Some((head, String::from_utf8_lossy(&body).to_string())))
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
}

fn bearer(head: &str) -> Option<String> {
    let value = head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    })?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(|rest| rest.trim().to_string())
}

async fn reply(stream: &mut TcpStream, status: u16, kind: &str, body: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}
