use crate::auth::{authorize, Auth, Carrier, Request};
use crate::client::Client;
use crate::home::Home;
use crate::ui::Ui;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Semaphore};
use tokio_rustls::TlsAcceptor;

const MAX_REQUEST: usize = 64 * 1024;
const DEFAULT_COLS: usize = 100;

/// Everything a `serve` invocation carries beyond its address: whether to wrap
/// the listener in TLS (with an optional PEM cert/key, else a dev self-signed
/// one) and the bearer-token gate every route runs through.
pub struct ServeConfig {
    pub https: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub auth: Auth,
    /// RFC 8d.2: leave the WebSocket carrier unscoped (base/barebone cross-env
    /// access). The default binds every WS connection to serve's env.
    pub admin: bool,
}

struct Ctx {
    ui: Arc<Mutex<Ui>>,
    sock: PathBuf,
    auth: Auth,
    /// The env every WebSocket carrier is bound to and the runtime token used to
    /// bind it (RFC 8d.2). `None` for an `--admin` serve, whose WS stays the
    /// unscoped base/barebone carrier.
    scope: Option<(String, String)>,
    /// RFC 8c ingress caps: the per-response streamed-byte ceiling and a permit
    /// pool bounding how many `/app` proxy connections run at once.
    max_body: usize,
    conns: Arc<Semaphore>,
}

/// The web carrier of RFC section 6b, hardened by P4.4: CGI-like one render per
/// request, plus a `/ws` upgrade for the WebSocket carrier, optional TLS, and
/// the single bearer authorizer in front of every route. Localhost only — the
/// page is the human gate, and production TLS/SSO stays a documented seam
/// (reverse proxy / JWT pass-through).
pub async fn serve(
    home: Option<PathBuf>,
    env: Option<String>,
    address: String,
    config: ServeConfig,
) -> Result<i32> {
    let home = Home::resolve(home)?;
    let loaded = crate::config::Config::load(&home.config_file()).ok();
    let root = loaded
        .as_ref()
        .map(|config| config.root_env.clone())
        .unwrap_or_else(|| "root".to_string());
    let ingress = loaded.map(|config| config.ingress).unwrap_or_default();
    let env = env.unwrap_or(root);
    if !config.https && !config.auth.is_public() && !config.auth.has_token() {
        bail!("serve over http needs --auth-token or --public (ingress is the public seam)");
    }
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind {address}"))?;
    let bound = listener.local_addr()?;
    if !bound.ip().is_loopback() {
        bail!("tenon serve binds loopback addresses only, not {bound}");
    }
    let acceptor = match config.https {
        true => Some(TlsAcceptor::from(crate::tls::server_config(
            config.cert.clone(),
            config.key.clone(),
        )?)),
        false => None,
    };
    let scheme = if config.https { "https" } else { "http" };
    println!("tenon: env {env} on {scheme}://{bound}");
    let scope = match config.admin {
        true => None,
        false => {
            let token = std::fs::read_to_string(home.runtime_token_file(&env))
                .map(|token| token.trim().to_string())
                .unwrap_or_default();
            Some((env.clone(), token))
        }
    };
    let ctx = Arc::new(Ctx {
        ui: Arc::new(Mutex::new(Ui::new(env))),
        sock: home.sock(),
        auth: config.auth,
        scope,
        max_body: ingress.body_limit,
        conns: Arc::new(Semaphore::new(ingress.max_connections.max(1))),
    });
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            return Ok(0);
        };
        let (ctx, acceptor) = (ctx.clone(), acceptor.clone());
        tokio::spawn(async move {
            match acceptor {
                Some(acceptor) => {
                    if let Ok(tls) = acceptor.accept(stream).await {
                        let _ = handle(tls, ctx).await;
                    }
                }
                None => {
                    let _ = handle(stream, ctx).await;
                }
            }
        });
    }
}

async fn handle<S>(mut stream: S, ctx: Arc<Ctx>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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
    let token = bearer(&head).or_else(|| field(&query, "token"));
    let ws_upgrade = is_websocket(&head);
    // RFC 8c: `/app/<name>/*` proxies into that app's sandbox. Resolved and
    // authorized here (public apps skip the token) before any general route.
    if let Some((name, rest)) = app_route(&path, &query) {
        return app(
            stream,
            &ctx,
            ws_upgrade,
            token.as_deref(),
            &method,
            &name,
            &rest,
            &head,
            body.as_bytes(),
        )
        .await;
    }
    let carrier = if is_upgrade(&head, &path) {
        Carrier::Ws
    } else {
        Carrier::Http
    };
    let request = Request {
        token: token.as_deref(),
        public: false,
    };
    if let Err(reject) = authorize(carrier, &request, &ctx.auth) {
        return reply(&mut stream, 401, "text/plain", reject.message()).await;
    }
    if carrier == Carrier::Ws {
        let bind = match &ctx.scope {
            Some((env, token)) => crate::ws::Bind::Scoped(env.clone(), token.clone()),
            None => crate::ws::Bind::Admin,
        };
        return upgrade(stream, &head, ctx.sock.clone(), bind).await;
    }
    let mut client = match Client::connect(&ctx.sock).await {
        Ok(client) => client,
        Err(error) => return reply(&mut stream, 503, "text/plain", &error.to_string()).await,
    };
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => {
            let cols = field(&query, "cols")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(DEFAULT_COLS)
                .clamp(40, 400);
            let mut ui = ctx.ui.lock().await;
            ui.backfill(&mut client).await;
            let model = ui.model(&mut client).await?;
            reply(&mut stream, 200, "text/html", &tenon_ui::html(&model, cols)).await
        }
        ("POST", "/prompt") => {
            let text = field(&body, "text").unwrap_or_default();
            if !text.trim().is_empty() {
                let _ = ctx.ui.lock().await.prompt(&mut client, &text).await;
            }
            redirect(&mut stream).await
        }
        ("POST", "/rollback") => {
            let _ = ctx.ui.lock().await.rollback(&mut client).await;
            redirect(&mut stream).await
        }
        ("POST", path) if path.starts_with("/approve/") => {
            let id = path.trim_start_matches("/approve/").parse::<i64>().ok();
            let approve = field(&body, "decision").as_deref() != Some("deny");
            if let Some(id) = id {
                let _ = ctx.ui.lock().await.answer(&mut client, id, approve).await;
            }
            redirect(&mut stream).await
        }
        _ => reply(&mut stream, 404, "text/plain", "no such page").await,
    }
}

/// Completes the WebSocket handshake on the already-authorized stream, then
/// hands it to the transparent bridge. The accept key derives from the client's
/// `Sec-WebSocket-Key`; no body follows a GET upgrade.
async fn upgrade<S>(mut stream: S, head: &str, sock: PathBuf, bind: crate::ws::Bind) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(key) = header(head, "sec-websocket-key") else {
        return reply(&mut stream, 400, "text/plain", "missing Sec-WebSocket-Key").await;
    };
    let accept = crate::ws::accept_key(&key);
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    crate::ws::bridge(stream, sock, bind).await
}

fn is_upgrade(head: &str, path: &str) -> bool {
    path == "/ws" && is_websocket(head)
}

fn is_websocket(head: &str) -> bool {
    header(head, "upgrade")
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

/// `/app/<name>/<rest>` split into the app name and the path to forward (prefix
/// stripped, query re-appended). `None` for any path that is not an app route.
fn app_route(path: &str, query: &str) -> Option<(String, String)> {
    let tail = path.strip_prefix("/app/")?;
    let (name, rest) = match tail.split_once('/') {
        Some((name, rest)) => (name, format!("/{rest}")),
        None => (tail, "/".to_string()),
    };
    if name.is_empty() {
        return None;
    }
    let rest = if query.is_empty() {
        rest
    } else {
        format!("{rest}?{query}")
    };
    Some((name.to_string(), rest))
}

/// Resolve one `/app/<name>` route through base, authorize it (a `public` app
/// skips the token), then proxy — HTTP, or a WebSocket upgrade on the same path.
/// A name with no live lease is a clean 404 rather than a hung connection.
#[allow(clippy::too_many_arguments)]
async fn app<S>(
    mut stream: S,
    ctx: &Arc<Ctx>,
    upgrade: bool,
    token: Option<&str>,
    method: &str,
    name: &str,
    rest: &str,
    head: &str,
    body: &[u8],
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut client = match Client::connect(&ctx.sock).await {
        Ok(client) => client,
        Err(error) => return reply(&mut stream, 503, "text/plain", &error.to_string()).await,
    };
    let resolved = client
        .call("ingress.resolve", serde_json::json!({ "name": name }))
        .await;
    let route = match resolved {
        Ok(route) if route["found"].as_bool().unwrap_or(false) => route,
        _ => {
            return reply(&mut stream, 404, "text/plain", "no such app route").await;
        }
    };
    let addr = route["addr"].as_str().unwrap_or_default().to_string();
    if addr.is_empty() {
        return reply(
            &mut stream,
            502,
            "text/plain",
            "ingress route has no address",
        )
        .await;
    }
    let public = route["public"].as_bool().unwrap_or(false);
    let env = route["env"].as_str().unwrap_or_default().to_string();
    let carrier = if upgrade { Carrier::Ws } else { Carrier::Http };
    let request = Request { token, public };
    if let Err(reject) = authorize(carrier, &request, &ctx.auth) {
        return reply(&mut stream, 401, "text/plain", reject.message()).await;
    }
    let Ok(_permit) = ctx.conns.clone().try_acquire_owned() else {
        return reply(&mut stream, 503, "text/plain", "ingress at connection cap").await;
    };
    let target = crate::proxy::Target {
        addr: &addr,
        app: name,
        env: &env,
        max_body: ctx.max_body,
    };
    if upgrade {
        crate::proxy::websocket(&mut stream, &target, rest, head).await
    } else {
        crate::proxy::http(&mut stream, &target, method, rest, head, body).await
    }
}

/// The bearer token out of an `Authorization: Bearer <token>` header.
fn bearer(head: &str) -> Option<String> {
    let value = header(head, "authorization")?;
    let rest = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    Some(rest.trim().to_string())
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Option<(String, String)>> {
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

async fn reply<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    kind: &str,
    body: &str,
) -> Result<()> {
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
async fn redirect<S: AsyncWrite + Unpin>(stream: &mut S) -> Result<()> {
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
        400 => "Bad Request",
        401 => "Unauthorized",
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

    #[test]
    fn reads_bearer_and_headers() {
        let head = "GET /ws HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer abc\r\nUpgrade: websocket\r\nSec-WebSocket-Key: k";
        assert_eq!(bearer(head).as_deref(), Some("abc"));
        assert_eq!(header(head, "upgrade").as_deref(), Some("websocket"));
        assert!(is_upgrade(head, "/ws"));
        assert!(!is_upgrade(head, "/"));
    }
}
