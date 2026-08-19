use crate::peer::Peer;
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use tokio::sync::oneshot;

pub enum Cmd {
    Boot {
        reply: oneshot::Sender<Result<(), String>>,
    },
    Register {
        peer: Peer,
        role: String,
        env: String,
        pid: i64,
        token: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Snapshot {
        reply: oneshot::Sender<Snapshot>,
    },
    PeerOf {
        env: String,
        reply: oneshot::Sender<Option<Peer>>,
    },
    Reset {
        env: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    SandboxExec {
        env: String,
        cmd: String,
        args: Vec<String>,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    SandboxDestroy {
        env: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    SandboxReaped {
        count: usize,
    },
    WorkerBoot {
        env: String,
    },
    HarnessBoot {
        env: String,
    },
    HarnessReady {
        env: String,
        pid: Option<i32>,
        error: Option<String>,
    },
    HarnessExit {
        env: String,
        generation: u64,
        code: Option<i32>,
    },
    EventsAppend {
        env: String,
        kind: String,
        data: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    EventsTail {
        env: String,
        after: i64,
        limit: i64,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    /// The P3.4 tables behind one variant rather than seven: `episodes.*`,
    /// `tool_results.*`, `blobs.*` and `state.retain` are all "one env's state
    /// file, one accessor, one JSON answer", and spelling each of them as its
    /// own `Cmd` would be seven identical shapes.
    Records {
        env: String,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    ConfigGet {
        env: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    ConfigPatch {
        env: String,
        target: String,
        patch: Value,
        approved: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    RuntimeRegister {
        env: String,
        params: Value,
        token: String,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    },
    RuntimeProbed {
        runtime: Box<crate::runtime::Runtime>,
        outcome: Result<i64, String>,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    },
    Approval {
        env: String,
        reason: String,
        kind: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    ApprovalList {
        status: Option<String>,
        limit: i64,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    ApprovalAnswer {
        id: i64,
        decision: String,
        note: Option<String>,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    ApprovalExpire {
        id: i64,
    },
    /// The prompt gate: a halted env and a killed base refuse with a reason
    /// instead of queueing a turn nobody will run.
    Guard {
        env: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Halt {
        env: String,
        reason: String,
    },
    Kill {
        on: bool,
        reason: String,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    },
    BudgetTick,
    Processes {
        env: String,
        count: i64,
    },
    WorkerReady {
        env: String,
        pid: Option<i64>,
        error: Option<String>,
    },
    SnapPull {
        env: String,
        reply: Option<oneshot::Sender<Result<Value, String>>>,
    },
    SnapList {
        env: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    SnapExport {
        env: String,
        path: String,
        approved: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    SnapPacked {
        env: String,
        step: i64,
        reference: String,
        bytes: Vec<u8>,
    },
    Spawn {
        peer: u64,
        parent: Option<String>,
        overrides: Value,
        approved: bool,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    RuntimeStop {
        env: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Restored {
        env: String,
        result: Value,
        error: Option<String>,
    },
    EnvStatus {
        env: String,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Stop {
        reply: oneshot::Sender<Result<Value, String>>,
    },
    AbortBoot {
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Subscribe {
        peer: Peer,
        env: Option<String>,
        reply: oneshot::Sender<Value>,
    },
    Gone {
        peer: u64,
    },
    Ready {
        reply: oneshot::Sender<bool>,
    },
}

pub struct NodeView {
    pub env: String,
    pub budget: Value,
    pub role: String,
    pub pid: Option<i32>,
    pub registered: bool,
    pub restarts: u32,
    pub sandbox: Option<Value>,
    pub peer: Option<Peer>,
    pub parent: Option<String>,
    pub depth: u32,
    pub children: Vec<String>,
    pub worker: Value,
    pub harness: Value,
    pub runtime: Option<Value>,
}

pub struct Snapshot {
    pub killed: Option<String>,
    pub home: PathBuf,
    pub release: PathBuf,
    pub pid: u32,
    pub exit_on_detach: bool,
    pub attached: usize,
    pub nodes: Vec<NodeView>,
}
