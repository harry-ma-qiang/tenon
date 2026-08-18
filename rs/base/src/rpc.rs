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
    pub role: String,
    pub pid: Option<i32>,
    pub registered: bool,
    pub restarts: u32,
    pub sandbox: Option<Value>,
    pub peer: Option<Peer>,
}

pub struct Snapshot {
    pub home: PathBuf,
    pub release: PathBuf,
    pub pid: u32,
    pub exit_on_detach: bool,
    pub attached: usize,
    pub nodes: Vec<NodeView>,
}
