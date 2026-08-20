use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
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

/// The human gate in front of a tool the profile lists under `gated_tools`.
/// `Ok(())` runs the call, an error is the reason the model reads as the tool
/// result. `ApiGate` asks base's approval queue; tests use a double.
pub trait Gate: Send + Sync + 'static {
    fn check<'a>(&'a self, name: &str, args: &Value) -> BoxFut<'a, Result<(), String>>;
}

/// The bridge to a mounted MCP server (RFC P4.7): a bridged tool is a tools-bus
/// row whose target service is `mcp`, and its execution is forwarded here by
/// qualified name (`<server>/<tool>`) after the tools/pre-execute waterfall and
/// the gate have run, so guard/budget/approval apply to a bridged tool exactly
/// as to a native one.
pub trait McpCall: Send + Sync + 'static {
    fn call<'a>(&'a self, qualified: &str, args: Value) -> BoxFut<'a, Answer>;
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

/// One tool call as the `tool_results` table records it: the `tool/result`
/// event it annotates, how long it took, and the blob holding the whole output
/// when that output was too large to keep in the event.
#[derive(Debug, Clone, Default)]
pub struct ToolRow {
    pub event_id: i64,
    pub name: String,
    pub status: String,
    pub duration_ms: i64,
    pub blob_hash: Option<String>,
}

/// One step of the loop as the `episodes` table records it. `user_event` is
/// the id of the user message the step is answering; base turns it and the
/// newest snapshot ref into the state hash.
#[derive(Debug, Clone, Default)]
pub struct EpisodeRow {
    pub session: String,
    pub step: i64,
    pub action: Value,
    pub verifier_score: f64,
    pub cost: Value,
    pub user_event: i64,
}

/// The append-only session log and the tables beside it. The harness never
/// opens sqlite: `BaseLog` speaks `events.append`, `log.query`,
/// `episodes.append`, `tool_results.append` and `blobs.put` to base, which
/// owns the file. The recording methods default to doing nothing, so a double
/// that only cares about the log stays a two-method implementation.
pub trait Log: Send + Sync + 'static {
    fn append<'a>(&'a self, kind: &str, data: Value) -> BoxFut<'a, Result<i64, String>>;
    fn tail<'a>(&'a self, after: i64, limit: i64) -> BoxFut<'a, Result<Vec<Event>, String>>;

    fn tool_result<'a>(&'a self, row: ToolRow) -> BoxFut<'a, Result<i64, String>> {
        let _ = row;
        Box::pin(async { Ok(0) })
    }

    fn episode<'a>(&'a self, row: EpisodeRow) -> BoxFut<'a, Result<i64, String>> {
        let _ = row;
        Box::pin(async { Ok(0) })
    }

    fn blob<'a>(&'a self, bytes: Vec<u8>) -> BoxFut<'a, Result<String, String>> {
        let _ = bytes;
        Box::pin(async { Err("blobs are not available".to_string()) })
    }
}
