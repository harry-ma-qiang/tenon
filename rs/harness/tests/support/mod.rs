#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tenon_harness::bus::{Answer, BoxFut, Bus, EpisodeRow, Event, Log, ToolRow};

#[derive(Default)]
pub struct FakeBus {
    pub calls: Mutex<Vec<(String, Vec<Value>)>>,
    pub svcs: Mutex<Vec<(String, String, Vec<Value>)>>,
    pub logs: Mutex<Vec<String>>,
    hooks: Mutex<HashMap<String, Value>>,
    services: Mutex<HashMap<String, Answer>>,
}

impl FakeBus {
    pub fn hook(&self, event: &str, answer: Value) {
        self.hooks.lock().unwrap().insert(event.to_string(), answer);
    }

    pub fn service(&self, name: &str, method: &str, answer: Answer) {
        self.services
            .lock()
            .unwrap()
            .insert(format!("{name}.{method}"), answer);
    }

    pub fn seen(&self, name: &str, method: &str) -> usize {
        self.svcs
            .lock()
            .unwrap()
            .iter()
            .filter(|(service, called, _)| service == name && called == method)
            .count()
    }
}

impl Bus for FakeBus {
    fn svc<'a>(&'a self, name: &str, method: &str, args: Vec<Value>) -> BoxFut<'a, Answer> {
        self.svcs
            .lock()
            .unwrap()
            .push((name.to_string(), method.to_string(), args.clone()));
        let answer = self
            .services
            .lock()
            .unwrap()
            .get(&format!("{name}.{method}"))
            .cloned()
            .unwrap_or_else(|| Ok(json!({"ok": true})));
        Box::pin(async move { answer })
    }

    fn call<'a>(&'a self, event: &str, args: Vec<Value>) -> BoxFut<'a, Answer> {
        self.calls
            .lock()
            .unwrap()
            .push((event.to_string(), args.clone()));
        let hook = self.hooks.lock().unwrap().get(event).cloned();
        let answer = hook.unwrap_or(Value::Array(args));
        Box::pin(async move { Ok(answer) })
    }

    fn emit(&self, _event: &str, _args: Vec<Value>) {}

    fn log(&self, message: String) {
        self.logs.lock().unwrap().push(message);
    }
}

#[derive(Default)]
pub struct MemLog {
    pub rows: Mutex<Vec<Event>>,
    pub episodes: Mutex<Vec<EpisodeRow>>,
    pub tools: Mutex<Vec<ToolRow>>,
    pub blobs: Mutex<Vec<Vec<u8>>>,
}

impl MemLog {
    pub fn kinds(&self) -> Vec<String> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.kind.clone())
            .collect()
    }

    pub fn of(&self, kind: &str) -> Vec<Value> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.kind == kind)
            .map(|event| event.data.clone())
            .collect()
    }

    pub fn seed(&self, kind: &str, data: Value) {
        let mut rows = self.rows.lock().unwrap();
        let id = rows.len() as i64 + 1;
        rows.push(Event {
            id,
            at: id,
            kind: kind.to_string(),
            data,
        });
    }
}

impl MemLog {
    pub fn episodes(&self) -> Vec<EpisodeRow> {
        self.episodes.lock().unwrap().clone()
    }

    pub fn tools(&self) -> Vec<ToolRow> {
        self.tools.lock().unwrap().clone()
    }
}

impl Log for MemLog {
    fn append<'a>(&'a self, kind: &str, data: Value) -> BoxFut<'a, Result<i64, String>> {
        self.seed(kind, data);
        let id = self.rows.lock().unwrap().len() as i64;
        Box::pin(async move { Ok(id) })
    }

    fn episode<'a>(&'a self, row: EpisodeRow) -> BoxFut<'a, Result<i64, String>> {
        self.episodes.lock().unwrap().push(row);
        let id = self.episodes.lock().unwrap().len() as i64;
        Box::pin(async move { Ok(id) })
    }

    fn tool_result<'a>(&'a self, row: ToolRow) -> BoxFut<'a, Result<i64, String>> {
        self.tools.lock().unwrap().push(row);
        let id = self.tools.lock().unwrap().len() as i64;
        Box::pin(async move { Ok(id) })
    }

    fn blob<'a>(&'a self, bytes: Vec<u8>) -> BoxFut<'a, Result<String, String>> {
        let hash = format!("blob{}", bytes.len());
        self.blobs.lock().unwrap().push(bytes);
        Box::pin(async move { Ok(hash) })
    }

    fn tail<'a>(&'a self, after: i64, limit: i64) -> BoxFut<'a, Result<Vec<Event>, String>> {
        let rows: Vec<Event> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.id > after)
            .take(limit.max(0) as usize)
            .cloned()
            .collect();
        Box::pin(async move { Ok(rows) })
    }
}

pub async fn settle<F>(label: &str, mut done: F)
where
    F: FnMut() -> bool,
{
    for _ in 0..600 {
        if done() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{label} never happened");
}

pub fn llm(base_url: &str) -> Arc<tenon_harness::llm::Client> {
    Arc::new(tenon_harness::llm::Client::new(
        tenon_harness::config::Llm {
            base_url: base_url.to_string(),
            model: "fake-model".to_string(),
            retry_base_ms: 10,
            ..Default::default()
        },
    ))
}
