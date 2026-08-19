use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 1_048_576;

pub async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut head = [0u8; 4];
    match reader.read_exact(&mut head).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let size = u32::from_be_bytes(head) as usize;
    if size > MAX_FRAME {
        bail!("frame_too_large: {size} bytes");
    }
    let mut body = vec![0u8; size];
    reader.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

pub async fn write<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Value) -> Result<()> {
    let body = serde_json::to_vec(frame)?;
    if body.len() > MAX_FRAME {
        bail!("frame_too_large: {} bytes", body.len());
    }
    writer.write_all(&(body.len() as u32).to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

/// One reply frame. The correlation key is `id` on the front door and `req` on
/// the plugin wire; nothing else about the two shapes differs, so both are
/// built here rather than by hand at every handler.
pub fn rep(key: &str, id: Value, outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(result) => json!({"t": "rep", key: id, "result": result}),
        Err(error) => json!({"t": "rep", key: id, "error": error}),
    }
}

pub fn rep_id(id: u64, outcome: Result<Value, String>) -> Value {
    rep("id", json!(id), outcome)
}

pub fn rep_req(req: Value, outcome: Result<Value, String>) -> Value {
    rep("req", req, outcome)
}

pub fn method(frame: &Value) -> Option<&str> {
    frame.get("t").and_then(Value::as_str)
}

pub fn id(frame: &Value) -> Option<u64> {
    frame.get("id").and_then(Value::as_u64)
}

pub fn outcome(frame: &Value) -> Result<Value, String> {
    match frame.get("error") {
        Some(error) => Err(error.as_str().unwrap_or("error").to_string()),
        None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(frame: &Value) -> Vec<u8> {
        serde_json::to_vec(frame).expect("frame")
    }

    #[test]
    fn the_helpers_build_the_frames_the_handlers_built_by_hand() {
        assert_eq!(
            bytes(&rep_id(7, Ok(json!({"ok": true})))),
            bytes(&json!({"t": "rep", "id": 7, "result": {"ok": true}}))
        );
        assert_eq!(
            bytes(&rep_id(7, Err("boom".to_string()))),
            bytes(&json!({"t": "rep", "id": 7, "error": "boom"}))
        );
        assert_eq!(
            bytes(&rep_req(json!(3), Ok(json!("ok")))),
            bytes(&json!({"t": "rep", "req": 3, "result": "ok"}))
        );
        assert_eq!(
            bytes(&rep_req(Value::Null, Err("unknown method a.b".to_string()))),
            bytes(&json!({"t": "rep", "req": null, "error": "unknown method a.b"}))
        );
    }
}
