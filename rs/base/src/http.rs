use crate::client::Client;
use crate::home::Home;
use crate::ui::Ui;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

const MAX_REQUEST: usize = 64 * 1024;
const DEFAULT_COLS: usize = 100;

/// The web carrier of RFC section 6b: CGI-like, one render per request, no UI
/// state on the server and no JavaScript needed. Localhost only — the page is
/// the human gate, not a public surface — and hand-rolled on tokio rather than
/// pulling a web framework in for four routes.
pub async fn serve(home: Option<PathBuf>, env: Option<String>, address: String) -> Result<i32> {
    let home = Home::resolve(home)?;
    let root = crate::config::Config::load(&home.config_file())
        .map(|config| config.root_env)
        .unwrap_or_else(|_| "root".to_string());
    let env = env.unwrap_or(root);
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind {address}"))?;
    let bound = listener.local_addr()?;
    if !bound.ip().is_loopback() {
        bail!("tenon serve --http binds loopback addresses only, not {bound}");
    }
    println!("tenon: env {env} on http://{bound}");
    let ui = Arc::new(Mutex::new(Ui::new(env)));
    let sock = home.sock();
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            return Ok(0);
        };
        let (ui, sock) = (ui.clone(), sock.clone());
        tokio::spawn(async move {
            let _ = handle(stream, ui, sock).await;
        });
    }
}

async fn handle(mut stream: TcpStream, ui: Arc<Mutex<Ui>>, sock: PathBuf) -> Result<()> {
    let Some((head, body)) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let mut lines = head.lines();
    let request = lines.next().unwrap_or_default().to_string();
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };
    let mut client = match Client::connect(&sock).await {
        Ok(client) => client,
        Err(error) => return reply(&mut stream, 503, "text/plain", &error.to_string()).await,
    };
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => {
            let cols = field(&query, "cols")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(DEFAULT_COLS)
                .clamp(40, 400);
            let model = ui.lock().await.model(&mut client).await?;
            reply(&mut stream, 200, "text/html", &tenon_ui::html(&model, cols)).await
        }
        ("POST", "/prompt") => {
            let text = field(&body, "text").unwrap_or_default();
            if !text.trim().is_empty() {
                let _ = ui.lock().await.prompt(&mut client, &text).await;
            }
            redirect(&mut stream).await
        }
        ("POST", "/rollback") => {
            let _ = ui.lock().await.rollback(&mut client).await;
            redirect(&mut stream).await
        }
        ("POST", path) if path.starts_with("/approve/") => {
            let id = path.trim_start_matches("/approve/").parse::<i64>().ok();
            let approve = field(&body, "decision").as_deref() != Some("deny");
            if let Some(id) = id {
                let _ = ui.lock().await.answer(&mut client, id, approve).await;
            }
            redirect(&mut stream).await
        }
        _ => reply(&mut stream, 404, "text/plain", "no such page").await,
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

/// One field out of a query string or an `application/x-www-form-urlencoded`
/// body, percent-decoded, `+` as space.
fn field(body: &str, name: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (decode(key) == name).then(|| decode(value))
    })
}

fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn reply(stream: &mut TcpStream, status: u16, kind: &str, body: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {kind}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        phrase(status),
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// Every POST answers with a redirect to `GET /`, so a reload never repeats
/// the action and the page keeps no state of its own.
async fn redirect(stream: &mut TcpStream) -> Result<()> {
    let head =
        "HTTP/1.1 303 See Other\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_form_fields() {
        assert_eq!(field("text=hi+there", "text").as_deref(), Some("hi there"));
        assert_eq!(
            field("a=1&decision=deny", "decision").as_deref(),
            Some("deny")
        );
        assert_eq!(field("text=a%20b%21", "text").as_deref(), Some("a b!"));
        assert_eq!(field("cols=120", "rows"), None);
    }
}
