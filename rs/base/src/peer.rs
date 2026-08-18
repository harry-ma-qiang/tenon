use crate::frame;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

type Answer = Result<Value, String>;
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Answer>>>>;

#[derive(Clone)]
pub struct Peer {
    id: u64,
    tx: mpsc::UnboundedSender<Value>,
    waiting: Waiting,
    next: Arc<AtomicU64>,
}

impl Peer {
    pub fn new(id: u64, tx: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            id,
            tx,
            waiting: Arc::new(Mutex::new(HashMap::new())),
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn send(&self, frame: Value) {
        let _ = self.tx.send(frame);
    }

    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Answer {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut body = json!({ "t": method, "id": id });
        if let (Some(target), Some(extra)) = (body.as_object_mut(), params.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().expect("peer lock").insert(id, tx);
        if self.tx.send(body).is_err() {
            self.waiting.lock().expect("peer lock").remove(&id);
            return Err("node_gone".to_string());
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err("node_gone".to_string()),
            Err(_) => {
                self.waiting.lock().expect("peer lock").remove(&id);
                Err("timeout".to_string())
            }
        }
    }

    pub fn resolve(&self, frame: &Value) {
        let Some(id) = frame::id(frame) else { return };
        let waiter = self.waiting.lock().expect("peer lock").remove(&id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(frame::outcome(frame));
        }
    }

    pub fn fail_all(&self, reason: &str) {
        let waiting: Vec<_> = self
            .waiting
            .lock()
            .expect("peer lock")
            .drain()
            .map(|(_, waiter)| waiter)
            .collect();
        for waiter in waiting {
            let _ = waiter.send(Err(reason.to_string()));
        }
    }
}
