use anyhow::{bail, Result};
use serde_json::Value;
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
