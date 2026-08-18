use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type Fut<T> = BoxFut<'static, T>;
pub type Answer = Result<Value, String>;

/// The kernel bus as the harness needs it: call a service, run a waterfall,
/// fire an event. `Wire` is the real one; tests use a recording double.
pub trait Bus: Send + Sync + 'static {
    fn svc<'a>(&'a self, name: &str, method: &str, args: Vec<Value>) -> BoxFut<'a, Answer>;
    fn call<'a>(&'a self, event: &str, args: Vec<Value>) -> BoxFut<'a, Answer>;
    fn emit(&self, event: &str, args: Vec<Value>);
    fn log(&self, message: String) {
        eprintln!("{message}");
    }
    fn max_frame(&self) -> usize {
        1_048_576
    }
}

/// One row of the session log. `id` is the rowid in the env's state file, so
/// it doubles as the replay offset.
#[derive(Debug, Clone)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub data: Value,
}

/// The append-only session log. The harness never opens sqlite: `BaseLog`
/// speaks `events.append` / `events.tail` to base, which owns the file.
pub trait Log: Send + Sync + 'static {
    fn append<'a>(&'a self, kind: &str, data: Value) -> BoxFut<'a, Result<i64, String>>;
    fn tail<'a>(&'a self, after: i64, limit: i64) -> BoxFut<'a, Result<Vec<Event>, String>>;
}
