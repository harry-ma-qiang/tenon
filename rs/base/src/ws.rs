use crate::frame;
use anyhow::{bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// How a bridged WebSocket connection is bound to base's front door (RFC 8d.2):
/// `Scoped(env, token)` calls `auth.scope` first, so every RPC the browser
/// client then issues is confined to that env exactly like a per-env plugin;
/// `Admin` leaves the connection unscoped, the base/barebone cross-env carrier a
/// serve started explicitly with `--admin` grants.
pub enum Bind {
    Scoped(String, String),
    Admin,
}

/// The WebSocket carrier of RFC P4.4: the 5th wire transport, same frames, no
/// new protocol. After the HTTP upgrade the connection is bridged transparently
/// to base's own front door — each text frame is one front-door request or a
/// pushed `t:"ev"` envelope, so `bus.subscribe` streams and every RPC ride the
/// exact same shapes the UDS carrier uses. Binary frames are reserved for media
/// (accepted and ignored for now). A `Scoped` bind binds the carrier to serve's
/// env before any client frame is forwarded, so the connection cannot name or
/// reach another env.
pub async fn bridge<S>(stream: S, sock: PathBuf, bind: Bind) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let socket = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;
    let (mut ws_tx, mut ws_rx) = socket.split();
    let uds = UnixStream::connect(&sock).await?;
    let (mut reader, mut writer) = uds.into_split();

    if let Bind::Scoped(env, token) = bind {
        let request = json!({"t": "auth.scope", "id": 0, "env": env, "token": token});
        frame::write(&mut writer, &request).await?;
        match frame::read(&mut reader).await? {
            Some(reply) if reply.get("error").is_none() => {}
            _ => {
                let _ = ws_tx.send(Message::Close(None)).await;
                bail!("ws carrier could not bind to env {env}");
            }
        }
    }

    let uplink = tokio::spawn(async move {
        while let Some(message) = ws_rx.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    if frame::write(&mut writer, &value).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Binary(_)) => continue,
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    });

    while let Ok(Some(value)) = frame::read(&mut reader).await {
        if ws_tx.send(Message::Text(value.to_string())).await.is_err() {
            break;
        }
    }
    uplink.abort();
    let _ = ws_tx.send(Message::Close(None)).await;
    Ok(())
}

/// The `Sec-WebSocket-Accept` value for a client's `Sec-WebSocket-Key`.
pub fn accept_key(key: &str) -> String {
    tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes())
}
