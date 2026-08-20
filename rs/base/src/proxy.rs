use anyhow::Result;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// The `/app/<name>` reverse proxy of RFC 8c (P4.5). One host, one route: strip
/// the `/app/<name>` prefix, stamp `X-Tenon-App`/`X-Tenon-Env`, forward the
/// request to the sandbox address the ingress registry resolved, and stream the
/// response back. No subdomains, no per-app TLS, no load balancing.
///
/// A route target (`X-Tenon-*`) the client tried to set is dropped, and the
/// platform bearer token is never forwarded to the app.
pub struct Target<'a> {
    pub addr: &'a str,
    pub app: &'a str,
    pub env: &'a str,
    pub max_body: usize,
}

/// One buffered request forwarded to the app, its response streamed back. The
/// app speaks `Connection: close`, so a read to EOF is the whole response.
pub async fn http<S>(
    client: &mut S,
    target: &Target<'_>,
    method: &str,
    path: &str,
    head: &str,
    body: &[u8],
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut upstream = match TcpStream::connect(target.addr).await {
        Ok(stream) => stream,
        Err(error) => return bad_gateway(client, &error.to_string()).await,
    };
    let mut request = format!("{method} {path} HTTP/1.0\r\n");
    request.push_str(&forwarded_headers(head, target, false));
    request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    request.push_str("Connection: close\r\n\r\n");
    upstream.write_all(request.as_bytes()).await?;
    upstream.write_all(body).await?;
    upstream.flush().await?;

    let mut buffer = [0u8; 16 * 1024];
    let mut total = 0usize;
    loop {
        let read = upstream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total += read;
        if total > target.max_body {
            break;
        }
        if client.write_all(&buffer[..read]).await.is_err() {
            break;
        }
    }
    let _ = client.flush().await;
    Ok(())
}

/// A WebSocket upgrade passed straight through on the same route (RFC 8c). The
/// handshake and every frame after it are opaque bytes once the upgrade request
/// carries the `X-Tenon-*` headers, so the two halves are copied verbatim.
pub async fn websocket<S>(client: &mut S, target: &Target<'_>, path: &str, head: &str) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut upstream = match TcpStream::connect(target.addr).await {
        Ok(stream) => stream,
        Err(error) => return bad_gateway(client, &error.to_string()).await,
    };
    let mut request = format!("GET {path} HTTP/1.1\r\n");
    request.push_str(&forwarded_headers(head, target, true));
    request.push_str("\r\n");
    upstream.write_all(request.as_bytes()).await?;
    upstream.flush().await?;
    let _ = copy_bidirectional(client, &mut upstream).await;
    Ok(())
}

/// The request headers as forwarded: the original set minus `Host` (replaced),
/// `Authorization` (the app never sees the platform token), any client-set
/// `X-Tenon-*` (no route spoofing), `Content-Length` and `Connection` (this
/// carrier sets its own), plus the two identity headers. A WebSocket forward
/// keeps `Connection`/`Upgrade`/`Sec-WebSocket-*` so the handshake survives.
fn forwarded_headers(head: &str, target: &Target<'_>, keep_upgrade: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!("Host: {}\r\n", target.addr));
    for line in head.lines().skip(1) {
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let drop = name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("content-length")
            || name.to_ascii_lowercase().starts_with("x-tenon-")
            || (!keep_upgrade && name.eq_ignore_ascii_case("connection"));
        if drop {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(&format!("X-Tenon-App: {}\r\n", target.app));
    out.push_str(&format!("X-Tenon-Env: {}\r\n", target.env));
    out
}

async fn bad_gateway<S: AsyncWrite + Unpin>(client: &mut S, reason: &str) -> Result<()> {
    let body = format!("ingress: app unreachable ({reason})");
    let head = format!(
        "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    client.write_all(head.as_bytes()).await?;
    client.write_all(body.as_bytes()).await?;
    client.flush().await?;
    Ok(())
}
