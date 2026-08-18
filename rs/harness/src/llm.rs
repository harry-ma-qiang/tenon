use crate::bus::{Answer, Bus};
use crate::config::Llm;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const CHUNK_FLUSH: usize = 240;

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt: i64,
    pub completion: i64,
    pub total: i64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.prompt += other.prompt;
        self.completion += other.completion;
        self.total += other.total;
    }

    pub fn json(&self) -> Value {
        json!({"prompt": self.prompt, "completion": self.completion, "total": self.total})
    }

    fn read(value: &Value) -> Option<Usage> {
        let object = value.as_object()?;
        Some(Usage {
            prompt: object
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            completion: object
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
            total: object
                .get("total_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<Value>,
    pub finish: String,
    pub usage: Usage,
}

impl Reply {
    /// The assistant message exactly as it goes back into the next request.
    pub fn message(&self) -> Value {
        let mut message = json!({"role": "assistant", "content": self.content});
        if !self.tool_calls.is_empty() {
            message["tool_calls"] = json!(self.tool_calls);
        }
        message
    }

    pub fn json(&self) -> Value {
        json!({
            "message": self.message(),
            "finish_reason": self.finish,
            "usage": self.usage.json(),
        })
    }
}

pub struct Client {
    config: Llm,
    key: Option<String>,
    http: reqwest::Client,
}

impl Client {
    pub fn new(config: Llm) -> Self {
        let key = std::env::var(&config.api_key_env)
            .ok()
            .filter(|k| !k.is_empty());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .build()
            .unwrap_or_default();
        Self { config, key, http }
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn has_key(&self) -> bool {
        self.key.is_some() || self.config.base_url.starts_with("http://")
    }

    pub fn describe(&self) -> Value {
        json!({
            "provider": self.config.provider,
            "base_url": self.config.base_url,
            "model": self.config.model,
            "api_key_env": self.config.api_key_env,
            "key": self.key.is_some(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.config.base_url.trim_end_matches('/'))
    }

    pub fn request(&self, messages: Vec<Value>, tools: Vec<Value>, stream: bool) -> Value {
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": stream,
            "temperature": self.config.temperature,
        });
        if stream {
            body["stream_options"] = json!({"include_usage": true});
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }
        body
    }

    pub async fn models(&self) -> Answer {
        let response = self
            .send(reqwest::Method::GET, &self.url("models"), None)
            .await
            .map_err(|error| error.to_string())?;
        let body: Value = response.json().await.map_err(|error| error.to_string())?;
        Ok(body)
    }

    /// One chat completion with retry. `sink` sees every streamed text delta,
    /// coalesced so a token-per-frame provider does not become a row per token.
    pub async fn chat<F>(&self, body: &Value, mut sink: F) -> Result<Reply, String>
    where
        F: FnMut(&str),
    {
        let attempts = self.config.retry_attempts.max(1);
        let mut last = String::new();
        for attempt in 0..attempts {
            if attempt > 0 {
                let wait = self.config.retry_base_ms * (1 << (attempt - 1).min(5));
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            match self.once(body, &mut sink).await {
                Ok(reply) => return Ok(reply),
                Err(Fail::Fatal(reason)) => return Err(reason),
                Err(Fail::Retry(reason)) => last = reason,
            }
        }
        Err(format!(
            "llm request failed after {attempts} attempts: {last}"
        ))
    }

    async fn once<F>(&self, body: &Value, sink: &mut F) -> Result<Reply, Fail>
    where
        F: FnMut(&str),
    {
        let response = self
            .send(
                reqwest::Method::POST,
                &self.url("chat/completions"),
                Some(body),
            )
            .await
            .map_err(|error| match error.is_timeout() || error.is_connect() {
                true => Fail::Retry(error.to_string()),
                false => Fail::Fatal(error.to_string()),
            })?;
        let status = response.status().as_u16();
        if status != 200 {
            let text = response.text().await.unwrap_or_default();
            let reason = format!(
                "http {status}: {}",
                text.trim().chars().take(400).collect::<String>()
            );
            return match status == 429 || status >= 500 {
                true => Err(Fail::Retry(reason)),
                false => Err(Fail::Fatal(reason)),
            };
        }
        match body.get("stream").and_then(Value::as_bool).unwrap_or(false) {
            true => stream(response, sink).await,
            false => {
                let value: Value = response
                    .json()
                    .await
                    .map_err(|error| Fail::Retry(error.to_string()))?;
                whole(&value)
            }
        }
    }

    async fn send(
        &self,
        verb: reqwest::Method,
        url: &str,
        body: Option<&Value>,
    ) -> reqwest::Result<reqwest::Response> {
        let mut request = self.http.request(verb, url);
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        request.send().await
    }
}

enum Fail {
    Retry(String),
    Fatal(String),
}

fn whole(value: &Value) -> Result<Reply, Fail> {
    let choice = &value["choices"][0];
    let message = &choice["message"];
    Ok(Reply {
        content: message["content"].as_str().unwrap_or_default().to_string(),
        tool_calls: message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        finish: choice["finish_reason"]
            .as_str()
            .unwrap_or("stop")
            .to_string(),
        usage: Usage::read(&value["usage"]).unwrap_or_default(),
    })
}

async fn stream<F>(response: reqwest::Response, sink: &mut F) -> Result<Reply, Fail>
where
    F: FnMut(&str),
{
    let mut response = response;
    let mut buffer = String::new();
    let mut reply = Reply::default();
    let mut calls: Vec<Value> = Vec::new();
    let mut pending = String::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| Fail::Retry(error.to_string()))?;
        let Some(chunk) = chunk else { break };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(cut) = buffer.find('\n') {
            let line = buffer[..cut].trim_end_matches('\r').to_string();
            buffer.drain(..cut + 1);
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            fold(&event, &mut reply, &mut calls, &mut pending);
            if pending.len() >= CHUNK_FLUSH {
                sink(&pending);
                pending.clear();
            }
        }
    }
    if !pending.is_empty() {
        sink(&pending);
    }
    reply.tool_calls = calls;
    Ok(reply)
}

fn fold(event: &Value, reply: &mut Reply, calls: &mut Vec<Value>, pending: &mut String) {
    if let Some(usage) = Usage::read(&event["usage"]) {
        reply.usage = usage;
    }
    let choice = &event["choices"][0];
    if let Some(finish) = choice["finish_reason"].as_str() {
        reply.finish = finish.to_string();
    }
    let delta = &choice["delta"];
    if let Some(text) = delta["content"].as_str() {
        reply.content.push_str(text);
        pending.push_str(text);
    }
    let Some(parts) = delta["tool_calls"].as_array() else {
        return;
    };
    for part in parts {
        let index = part["index"].as_u64().unwrap_or(0) as usize;
        while calls.len() <= index {
            calls.push(json!({
                "id": "",
                "type": "function",
                "function": {"name": "", "arguments": ""},
            }));
        }
        let slot = &mut calls[index];
        if let Some(id) = part["id"].as_str() {
            slot["id"] = json!(id);
        }
        if let Some(name) = part["function"]["name"].as_str() {
            let current = slot["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            slot["function"]["name"] = json!(format!("{current}{name}"));
        }
        if let Some(args) = part["function"]["arguments"].as_str() {
            let current = slot["function"]["arguments"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            slot["function"]["arguments"] = json!(format!("{current}{args}"));
        }
    }
}

/// `llm/request` is a waterfall: a hook may rewrite the request (an array back,
/// loader-identity semantics) or short-circuit the model entirely by answering
/// with an object, which is then taken as the reply.
pub async fn waterfall(bus: &Arc<dyn Bus>, request: Value) -> (Value, Option<Reply>) {
    match bus.call("llm/request", vec![request.clone()]).await {
        Ok(Value::Array(items)) => (items.into_iter().next().unwrap_or(request), None),
        Ok(Value::Object(map)) => {
            let short = Value::Object(map);
            let reply = whole(&short).ok().or_else(|| {
                Some(Reply {
                    content: short["content"].as_str().unwrap_or_default().to_string(),
                    ..Reply::default()
                })
            });
            (request, reply)
        }
        _ => (request, None),
    }
}
