use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One scripted answer of the fake OpenAI-compatible server. Enough to drive
/// the harness end to end without a model: a text reply, a tool call, or a
/// failure status the retry path has to survive.
#[derive(Debug, Clone)]
pub enum Say {
    Text(String),
    Tool(String, Value),
    Status(u16),
}

pub struct Fake {
    pub base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    script: Arc<Mutex<VecDeque<Say>>>,
}

impl Fake {
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().expect("fake lock").clone()
    }

    pub fn say(&self, more: Vec<Say>) {
        let mut script = self.script.lock().expect("fake lock");
        for item in more {
            script.push_back(item);
        }
    }
}

/// Binds 127.0.0.1 on a free port and serves `/chat/completions` from the
/// script, one entry per request; an exhausted script keeps answering "ok".
pub async fn spawn(script: Vec<Say>) -> std::io::Result<Fake> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let queue = Arc::new(Mutex::new(VecDeque::from(script)));
    let fake = Fake {
        base_url: format!("http://127.0.0.1:{port}"),
        requests: requests.clone(),
        script: queue.clone(),
    };
    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                return;
            };
            let requests = requests.clone();
            let queue = queue.clone();
            tokio::spawn(async move {
                let _ = handle(stream, requests, queue).await;
            });
        }
    });
    Ok(fake)
}

async fn handle(
    mut stream: TcpStream,
    requests: Arc<Mutex<Vec<Value>>>,
    queue: Arc<Mutex<VecDeque<Say>>>,
) -> std::io::Result<()> {
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let (head, body) = loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&buffer[..read]);
        let text = String::from_utf8_lossy(&raw).to_string();
        let Some(cut) = text.find("\r\n\r\n") else {
            continue;
        };
        let head = text[..cut].to_string();
        let want = content_length(&head);
        let body = raw[cut + 4..].to_vec();
        if body.len() >= want {
            break (head, body);
        }
    };
    let request: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    if head.starts_with("GET /models") {
        return json_reply(&mut stream, 200, &json!({"data": [{"id": "fake"}]})).await;
    }
    requests.lock().expect("fake lock").push(request.clone());
    let say = queue
        .lock()
        .expect("fake lock")
        .pop_front()
        .unwrap_or_else(|| Say::Text("ok".to_string()));
    if let Say::Status(code) = say {
        return json_reply(
            &mut stream,
            code,
            &json!({"error": {"message": "scripted"}}),
        )
        .await;
    }
    let stream_mode = request
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match stream_mode {
        true => sse(&mut stream, &say).await,
        false => json_reply(&mut stream, 200, &whole(&say)).await,
    }
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

fn whole(say: &Say) -> Value {
    let message = match say {
        Say::Text(text) => json!({"role": "assistant", "content": text}),
        Say::Tool(name, args) => json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": "call_fake_1",
                "type": "function",
                "function": {"name": name, "arguments": args.to_string()},
            }],
        }),
        Say::Status(_) => json!({"role": "assistant", "content": ""}),
    };
    let finish = match say {
        Say::Tool(_, _) => "tool_calls",
        _ => "stop",
    };
    json!({
        "id": "fake",
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
    })
}

async fn sse(stream: &mut TcpStream, say: &Say) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                Cache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n";
    stream.write_all(head.as_bytes()).await?;
    for line in frames(say) {
        let piece = format!("data: {line}\n\n");
        stream
            .write_all(format!("{:x}\r\n{piece}\r\n", piece.len()).as_bytes())
            .await?;
        stream.flush().await?;
    }
    let tail = "data: [DONE]\n\n";
    stream
        .write_all(format!("{:x}\r\n{tail}\r\n0\r\n\r\n", tail.len()).as_bytes())
        .await?;
    stream.flush().await
}

/// Deliberately fragmented: text arrives a few characters at a time and a tool
/// call arrives as name first, then its arguments in two pieces, which is what
/// the streaming parser has to reassemble.
fn frames(say: &Say) -> Vec<String> {
    let mut rows = Vec::new();
    match say {
        Say::Text(text) => {
            let chars: Vec<char> = text.chars().collect();
            for piece in chars.chunks(3) {
                let body: String = piece.iter().collect();
                rows.push(delta(json!({"content": body})));
            }
            rows.push(finish("stop"));
        }
        Say::Tool(name, args) => {
            rows.push(delta(json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "call_fake_1",
                    "type": "function",
                    "function": {"name": name, "arguments": ""},
                }],
            })));
            let text = args.to_string();
            let cut = text.len() / 2;
            for part in [&text[..cut], &text[cut..]] {
                rows.push(delta(json!({
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": part},
                    }],
                })));
            }
            rows.push(finish("tool_calls"));
        }
        Say::Status(_) => rows.push(finish("stop")),
    }
    rows.push(
        json!({
            "id": "fake",
            "choices": [],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18},
        })
        .to_string(),
    );
    rows
}

fn delta(delta: Value) -> String {
    json!({"id": "fake", "choices": [{"index": 0, "delta": delta}]}).to_string()
}

fn finish(reason: &str) -> String {
    json!({"id": "fake", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
        .to_string()
}

async fn json_reply(stream: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}
