use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

pub fn raw_connect(sock: &Path) -> UnixStream {
    UnixStream::connect(sock).expect("connect raw socket")
}

pub fn send_raw(stream: &mut UnixStream, body: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn send_frame(stream: &mut UnixStream, frame: &Value) -> std::io::Result<()> {
    send_raw(stream, serde_json::to_vec(frame).unwrap().as_slice())
}

pub fn read_frame(stream: &mut UnixStream, timeout: Duration) -> std::io::Result<Value> {
    stream.set_read_timeout(Some(timeout))?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head)?;
    let size = u32::from_be_bytes(head) as usize;
    let mut body = vec![0u8; size];
    stream.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}
