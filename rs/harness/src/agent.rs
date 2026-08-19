use crate::bus::{Answer, Bus, EpisodeRow, Log, ToolRow};
use crate::llm::{self, Client, Reply, Usage};
use crate::prompt::Prompt;
use crate::tools::Tools;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const HISTORY_LIMIT: i64 = 5_000;

/// A tool output longer than this goes to `blobs` whole and is referenced from
/// the `tool_results` row by hash. The model keeps seeing the truncated view
/// the tools bus produces, not the blob.
const BLOB_MIN: usize = 4_096;

/// Base64 inflates by a third and a base frame is capped at 1 MiB, so anything
/// above this could not be put in one frame. The worker spills its own big
/// outputs to files well below that, which is what keeps this unreachable in
/// practice; a result that still lands here keeps its `tool_results` row and
/// loses only the blob.
const BLOB_MAX: usize = 700_000;

#[derive(Default)]
struct Session {
    messages: Vec<Value>,
    queue: VecDeque<String>,
    running: bool,
    turns: u64,
    steps: u64,
    usage: Usage,
    last: String,
    user_event: i64,
}

/// The agent loop as a plugin (seam 8): one turn per prompt, one step per model
/// call, tool calls dispatched through the bus. Context overflow is not handled
/// here — a model error fails the turn and is reported as `turn/end{ok:false}`.
pub struct Agent {
    bus: Arc<dyn Bus>,
    log: Arc<dyn Log>,
    llm: Arc<Client>,
    tools: Arc<Tools>,
    prompt: Arc<Prompt>,
    max_steps: usize,
    sessions: Mutex<HashMap<String, Session>>,
    seq: AtomicU64,
}

impl Agent {
    pub fn new(
        bus: Arc<dyn Bus>,
        log: Arc<dyn Log>,
        llm: Arc<Client>,
        tools: Arc<Tools>,
        prompt: Arc<Prompt>,
        max_steps: usize,
    ) -> Self {
        Self {
            bus,
            log,
            llm,
            tools,
            prompt,
            max_steps: max_steps.max(1),
            sessions: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
        }
    }

    pub async fn call(self: &Arc<Self>, method: &str, args: &[Value]) -> Answer {
        let params = args.first().cloned().unwrap_or(json!({}));
        let id = params
            .get("session_id")
            .or_else(|| params.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match method {
            "ping" => Ok(json!("pong")),
            "session.create" => self.create().await,
            "session.prompt" => {
                let text = params
                    .get("text")
                    .or_else(|| params.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.enqueue(&id, text).await
            }
            "session.status" => self.status(&id),
            "session.history" => self.history(&id, &params).await,
            "session.resume" => self.resume(&id).await,
            "sessions" => Ok(self.sessions_json()),
            other => Err(format!("unknown method {other}")),
        }
    }

    fn sessions_json(&self) -> Value {
        let sessions = self.sessions.lock().expect("session lock");
        json!({
            "sessions": sessions
                .iter()
                .map(|(id, session)| json!({
                    "session_id": id,
                    "turns": session.turns,
                    "running": session.running,
                    "messages": session.messages.len(),
                }))
                .collect::<Vec<Value>>(),
        })
    }

    async fn create(self: &Arc<Self>) -> Answer {
        let id = format!(
            "s{}-{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        self.sessions
            .lock()
            .expect("session lock")
            .insert(id.clone(), Session::default());
        self.event(&id, "session/created", json!({})).await;
        Ok(json!({"session_id": id}))
    }

    fn status(&self, id: &str) -> Answer {
        let sessions = self.sessions.lock().expect("session lock");
        let Some(session) = sessions.get(id) else {
            return Err(format!("unknown session {id}"));
        };
        Ok(json!({
            "session_id": id,
            "running": session.running,
            "queued": session.queue.len(),
            "turns": session.turns,
            "steps": session.steps,
            "messages": session.messages.len(),
            "usage": session.usage.json(),
            "last": session.last,
        }))
    }

    async fn history(&self, id: &str, params: &Value) -> Answer {
        let after = params.get("after").and_then(Value::as_i64).unwrap_or(0);
        let limit = params
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(HISTORY_LIMIT);
        let events = self.log.tail(after, limit).await?;
        let rows: Vec<Value> = events
            .into_iter()
            .filter(|event| id.is_empty() || event.data.get("session").and_then(Value::as_str) == Some(id))
            .map(|event| json!({"id": event.id, "at": event.at, "kind": event.kind, "data": event.data}))
            .collect();
        Ok(json!({"session_id": id, "count": rows.len(), "events": rows}))
    }

    /// Replay: the event log is the version history, so a restarted harness
    /// rebuilds the model context by folding the session's own events.
    async fn resume(self: &Arc<Self>, id: &str) -> Answer {
        if id.is_empty() {
            return Err("session.resume needs a session_id".to_string());
        }
        let events = self.log.tail(0, HISTORY_LIMIT).await?;
        let mut messages: Vec<Value> = Vec::new();
        let mut turns = 0u64;
        let mut user_event = 0i64;
        for event in events {
            if event.data.get("session").and_then(Value::as_str) != Some(id) {
                continue;
            }
            match event.kind.as_str() {
                "user/message" => {
                    user_event = event.id;
                    messages.push(json!({
                        "role": "user",
                        "content": event.data.get("text").cloned().unwrap_or(json!("")),
                    }));
                }
                "assistant/message" => {
                    if let Some(message) = event.data.get("message") {
                        messages.push(message.clone());
                    }
                }
                "tool/result" => messages.push(json!({
                    "role": "tool",
                    "tool_call_id": event.data.get("id").cloned().unwrap_or(json!("")),
                    "content": event.data.get("text").and_then(Value::as_str).unwrap_or_default(),
                })),
                "turn/end" => turns += 1,
                _ => {}
            }
        }
        let count = messages.len();
        let mut sessions = self.sessions.lock().expect("session lock");
        let session = sessions.entry(id.to_string()).or_default();
        session.messages = messages;
        session.turns = turns;
        session.user_event = user_event;
        Ok(json!({"session_id": id, "messages": count, "turns": turns}))
    }

    async fn enqueue(self: &Arc<Self>, id: &str, text: String) -> Answer {
        if text.trim().is_empty() {
            return Err("session.prompt needs text".to_string());
        }
        let start = {
            let mut sessions = self.sessions.lock().expect("session lock");
            let Some(session) = sessions.get_mut(id) else {
                return Err(format!("unknown session {id}"));
            };
            session.queue.push_back(text.clone());
            let start = !session.running;
            session.running = true;
            start
        };
        let event = self.event(id, "user/message", json!({"text": text})).await;
        if let Some(session) = self.sessions.lock().expect("session lock").get_mut(id) {
            session.user_event = event;
        }
        if start {
            let agent = self.clone();
            let id = id.to_string();
            tokio::spawn(async move { agent.pump(id).await });
        }
        Ok(json!({"ok": true, "session_id": id, "queued": true}))
    }

    async fn pump(self: Arc<Self>, id: String) {
        loop {
            let next = {
                let mut sessions = self.sessions.lock().expect("session lock");
                let Some(session) = sessions.get_mut(&id) else {
                    return;
                };
                match session.queue.pop_front() {
                    Some(text) => text,
                    None => {
                        session.running = false;
                        return;
                    }
                }
            };
            self.turn(&id, next).await;
        }
    }

    async fn turn(&self, id: &str, text: String) {
        let turn = {
            let mut sessions = self.sessions.lock().expect("session lock");
            let Some(session) = sessions.get_mut(id) else {
                return;
            };
            session
                .messages
                .push(json!({"role": "user", "content": text}));
            session.turns += 1;
            session.turns
        };
        self.event(id, "turn/start", json!({"turn": turn})).await;
        let outcome = self.steps(id, turn).await;
        let (ok, error, answer) = match outcome {
            Ok(answer) => (true, Value::Null, answer),
            Err(reason) => (false, json!(reason), String::new()),
        };
        let usage = {
            let sessions = self.sessions.lock().expect("session lock");
            sessions
                .get(id)
                .map(|s| s.usage.json())
                .unwrap_or(Value::Null)
        };
        if let Some(session) = self.sessions.lock().expect("session lock").get_mut(id) {
            session.last = answer.clone();
        }
        self.event(
            id,
            "turn/end",
            json!({"turn": turn, "ok": ok, "error": error, "text": answer, "usage": usage}),
        )
        .await;
    }

    async fn steps(&self, id: &str, turn: u64) -> Result<String, String> {
        let mut answer = String::new();
        for step in 1..=self.max_steps {
            self.event(id, "step/start", json!({"turn": turn, "step": step}))
                .await;
            let reply = self.step(id, step).await?;
            answer = reply.content.clone();
            let calls = reply.tool_calls.clone();
            let mut all_ok = true;
            for call in &calls {
                all_ok &= self.tool(id, call).await;
            }
            self.event(
                id,
                "step/end",
                json!({"turn": turn, "step": step, "tool_calls": calls.len()}),
            )
            .await;
            self.episode(id, step as i64, &calls, all_ok, reply.usage.json())
                .await;
            if calls.is_empty() && self.stopping(id, turn, step, &answer).await {
                return Ok(answer);
            }
        }
        Ok(answer)
    }

    /// One model call: assemble the prompt, collect the catalog, run the
    /// `agent/pre-step` waterfall, stream the answer into the log.
    async fn step(&self, id: &str, step: usize) -> Result<Reply, String> {
        let system = self.prompt.render();
        let tools = self.tools.schemas();
        let history = {
            let sessions = self.sessions.lock().expect("session lock");
            sessions
                .get(id)
                .map(|s| s.messages.clone())
                .unwrap_or_default()
        };
        let pre = json!({
            "session": id,
            "step": step,
            "system": system,
            "messages": history,
            "tools": tools.iter().filter_map(|tool| tool["function"]["name"].as_str()).collect::<Vec<&str>>(),
        });
        let pre = match self.bus.call("agent/pre-step", vec![pre.clone()]).await {
            Ok(Value::Array(items)) => items.into_iter().next().unwrap_or(pre),
            _ => pre,
        };
        let system = pre["system"].as_str().unwrap_or_default().to_string();
        let mut messages = vec![json!({"role": "system", "content": system})];
        messages.extend(
            pre["messages"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter(),
        );
        let request = self.llm.request(messages, tools, true);
        let (request, short) = llm::waterfall(&self.bus, request).await;
        let reply = match short {
            Some(reply) => reply,
            None => {
                let (chunks, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                let log = self.log.clone();
                let session = id.to_string();
                let pump = tokio::spawn(async move {
                    while let Some(text) = rx.recv().await {
                        let data = json!({"session": session, "step": step, "text": text});
                        let _ = log.append("assistant/chunk", data).await;
                    }
                });
                let reply = self
                    .llm
                    .chat(&request, |delta| {
                        let _ = chunks.send(delta.to_string());
                    })
                    .await;
                drop(chunks);
                let _ = pump.await;
                reply?
            }
        };
        {
            let mut sessions = self.sessions.lock().expect("session lock");
            if let Some(session) = sessions.get_mut(id) {
                session.messages.push(reply.message());
                session.usage.add(&reply.usage);
                session.steps += 1;
            }
        }
        self.event(
            id,
            "assistant/message",
            json!({
                "step": step,
                "message": reply.message(),
                "finish_reason": reply.finish,
                "usage": reply.usage.json(),
            }),
        )
        .await;
        Ok(reply)
    }

    /// Returns whether the call succeeded, which is what the step's episode
    /// scores. The whole output goes to `blobs` when it is large; the model
    /// still sees the tools bus's truncated view of it.
    async fn tool(&self, id: &str, call: &Value) -> bool {
        let started = std::time::Instant::now();
        let call_id = call["id"].as_str().unwrap_or_default().to_string();
        let name = call["function"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let raw = call["function"]["arguments"]
            .as_str()
            .unwrap_or("{}")
            .to_string();
        let args = serde_json::from_str::<Value>(&raw).unwrap_or(json!({}));
        self.event(
            id,
            "tool/call",
            json!({"id": call_id, "name": name, "arguments": args}),
        )
        .await;
        let outcome = self.tools.execute(&name, args, Some(call_id.clone())).await;
        let text = outcome.text();
        let whole = outcome.body();
        let hash = match (BLOB_MIN..=BLOB_MAX).contains(&whole.len()) {
            true => match self.log.blob(whole.into_bytes()).await {
                Ok(hash) => Some(hash),
                Err(error) => {
                    self.bus
                        .log(format!("tenon harness: output not stored: {error}"));
                    None
                }
            },
            false => None,
        };
        let event = self
            .event(
                id,
                "tool/result",
                json!({
                    "id": call_id,
                    "name": name,
                    "ok": outcome.ok,
                    "denied": outcome.denied,
                    "text": text,
                    "blob": hash,
                }),
            )
            .await;
        let status = match (outcome.ok, outcome.denied) {
            (_, true) => "denied",
            (true, _) => "ok",
            (false, _) => "error",
        };
        if let Err(error) = self
            .log
            .tool_result(ToolRow {
                event_id: event,
                name: name.clone(),
                status: status.to_string(),
                duration_ms: started.elapsed().as_millis() as i64,
                blob_hash: hash,
            })
            .await
        {
            self.bus
                .log(format!("tenon harness: tool result not recorded: {error}"));
        }
        let mut sessions = self.sessions.lock().expect("session lock");
        if let Some(session) = sessions.get_mut(id) {
            session.messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": text,
            }));
        }
        outcome.ok && !outcome.denied
    }

    /// One `episodes` row per step, written from day one so the navigator has
    /// data before it exists. The verifier is a placeholder: 1.0 when every
    /// tool call of the step came back ok (a step that only answers counts as
    /// ok), 0.0 otherwise. A real verifier arrives with P5/P6.
    async fn episode(&self, id: &str, step: i64, calls: &[Value], all_ok: bool, cost: Value) {
        let action = match calls.is_empty() {
            true => json!("respond"),
            false => json!(calls
                .iter()
                .map(|call| json!({
                    "name": call["function"]["name"],
                    "arguments": call["function"]["arguments"],
                }))
                .collect::<Vec<Value>>()),
        };
        let user_event = self
            .sessions
            .lock()
            .expect("session lock")
            .get(id)
            .map(|session| session.user_event)
            .unwrap_or(0);
        let row = EpisodeRow {
            session: id.to_string(),
            step,
            action,
            verifier_score: match all_ok {
                true => 1.0,
                false => 0.0,
            },
            cost,
            user_event,
        };
        if let Err(error) = self.log.episode(row).await {
            self.bus
                .log(format!("tenon harness: episode not recorded: {error}"));
        }
    }

    /// `agent/turn-stopping` is the veto seam: a hook answering `{stop: false}`
    /// (optionally with `text`) keeps the turn going with one more user message.
    async fn stopping(&self, id: &str, turn: u64, step: usize, answer: &str) -> bool {
        let request = json!({"session": id, "turn": turn, "step": step, "text": answer});
        let verdict = self.bus.call("agent/turn-stopping", vec![request]).await;
        let value = match verdict {
            Ok(Value::Array(items)) => items.into_iter().next().unwrap_or(Value::Null),
            Ok(other) => other,
            Err(_) => Value::Null,
        };
        if value.get("stop").and_then(Value::as_bool) == Some(false) {
            let text = value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("continue")
                .to_string();
            let mut sessions = self.sessions.lock().expect("session lock");
            if let Some(session) = sessions.get_mut(id) {
                session
                    .messages
                    .push(json!({"role": "user", "content": text}));
            }
            return false;
        }
        true
    }

    /// Answers with the row id the log gave the event, which is what a
    /// `tool_results` row points at and what an episode's state hash folds in.
    async fn event(&self, id: &str, kind: &str, mut data: Value) -> i64 {
        if let Some(object) = data.as_object_mut() {
            object.insert("session".to_string(), json!(id));
        }
        match self.log.append(kind, data).await {
            Ok(row) => row,
            Err(error) => {
                self.bus
                    .log(format!("tenon harness: event {kind} not logged: {error}"));
                0
            }
        }
    }
}
