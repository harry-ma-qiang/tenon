use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn label(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub name: String,
    pub role: String,
    pub status: String,
    pub sandbox: Option<String>,
    pub restarts: u32,
    pub children: Vec<NodeInfo>,
}

impl NodeInfo {
    pub fn new(
        name: impl Into<String>,
        role: impl Into<String>,
        status: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            role: role.into(),
            status: status.into(),
            sandbox: None,
            restarts: 0,
            children: Vec::new(),
        }
    }

    pub fn with_sandbox(mut self, sandbox: impl Into<String>) -> Self {
        self.sandbox = Some(sandbox.into());
        self
    }

    pub fn with_restarts(mut self, restarts: u32) -> Self {
        self.restarts = restarts;
        self
    }

    pub fn with_children(mut self, children: Vec<NodeInfo>) -> Self {
        self.children = children;
        self
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptItem {
    pub role: Role,
    pub text: String,
    pub tool_name: Option<String>,
    pub line_count: Option<usize>,
}

impl TranscriptItem {
    pub fn message(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            tool_name: None,
            line_count: None,
        }
    }

    pub fn tool(name: impl Into<String>, text: impl Into<String>) -> Self {
        let text = text.into();
        let line_count = text.lines().count().max(1);
        Self {
            role: Role::Tool,
            text,
            tool_name: Some(name.into()),
            line_count: Some(line_count),
        }
    }

    pub fn is_tool(&self) -> bool {
        self.tool_name.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct EventLine {
    pub ts: i64,
    pub kind: String,
    pub summary: String,
}

impl EventLine {
    pub fn new(ts: i64, kind: impl Into<String>, summary: impl Into<String>) -> Self {
        Self {
            ts,
            kind: kind.into(),
            summary: summary.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Approval {
    pub id: String,
    pub env: String,
    pub reason: String,
}

impl Approval {
    pub fn new(id: impl Into<String>, env: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            env: env.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub base_pid: u32,
    pub attached: usize,
    pub budgets: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct UiModel {
    pub envs: Vec<NodeInfo>,
    pub selected_session: Option<String>,
    pub transcript: Vec<TranscriptItem>,
    pub expanded: HashSet<usize>,
    pub events: Vec<EventLine>,
    pub approvals: Vec<Approval>,
    pub status: StatusLine,
    pub input_hint: String,
}

impl UiModel {
    pub fn new() -> Self {
        Self::default()
    }
}
