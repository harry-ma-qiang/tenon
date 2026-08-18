use crate::frame;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use tokio::net::UnixStream;

pub struct Client {
    stream: UnixStream,
    next: u64,
}

impl Client {
    pub async fn connect(sock: &Path) -> Result<Self> {
        let stream = UnixStream::connect(sock)
            .await
            .with_context(|| format!("connect {}: is the base running?", sock.display()))?;
        Ok(Self { stream, next: 1 })
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        let mut body = json!({ "t": method, "id": id });
        if let (Some(target), Some(extra)) = (body.as_object_mut(), params.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        frame::write(&mut self.stream, &body).await?;
        loop {
            match frame::read(&mut self.stream).await? {
                None => bail!("base closed the connection"),
                Some(answer) if frame::id(&answer) == Some(id) => {
                    return frame::outcome(&answer).map_err(|error| anyhow::anyhow!(error));
                }
                Some(_other) => continue,
            }
        }
    }

    pub async fn event(&mut self) -> Result<Option<Value>> {
        loop {
            match frame::read(&mut self.stream).await? {
                None => return Ok(None),
                Some(body) if frame::method(&body) == Some("event") => return Ok(Some(body)),
                Some(_other) => continue,
            }
        }
    }
}
