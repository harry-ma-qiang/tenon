use crate::client::Client;
use crate::params::{array, i64_or, opt_text, str_of, text, u64_or, value};
use anyhow::Result;
use serde_json::{json, Value};
use tenon_ui::{Approval, EventLine, NodeInfo, Role, StatusLine, TranscriptItem, UiModel};

const EVENT_TAIL: i64 = 200;
const TRANSCRIPT_MAX: usize = 200;
const SUMMARY_CHARS: usize = 200;

/// Everything the ASCII UI shows, read off base's front door: `status` for the
/// tree, `session.history` for the transcript, `events.tail` for the tail and
/// `approval.list` for the queue. Nothing here holds UI state — the carriers do.
pub struct Ui {
    pub env: String,
    pub session: Option<String>,
}

impl Ui {
    pub fn new(env: String) -> Self {
        Self { env, session: None }
    }

    pub async fn model(&self, client: &mut Client) -> Result<UiModel> {
        let status = client
            .call("status", json!({}))
            .await
            .unwrap_or(Value::Null);
        let events = client
            .call("events.tail", json!({"env": self.env, "limit": EVENT_TAIL}))
            .await
            .unwrap_or(Value::Null);
        let history = client
            .call(
                "session.history",
                json!({"env": self.env, "session_id": self.session.clone().unwrap_or_default()}),
            )
            .await
            .unwrap_or(Value::Null);
        let approvals = client
            .call("approval.list", json!({"status": "pending"}))
            .await
            .unwrap_or(Value::Null);
        Ok(build(&self.env, &status, &events, &history, &approvals))
    }

    pub async fn prompt(&mut self, client: &mut Client, text: &str) -> Result<Value> {
        if self.session.is_none() {
            let created = client
                .call("session.create", json!({"env": self.env}))
                .await?;
            self.session = opt_text(&created, "session_id");
        }
        let session = self.session.clone().unwrap_or_default();
        client
            .call(
                "session.prompt",
                json!({"env": self.env, "session_id": session, "text": text}),
            )
            .await
    }

    pub async fn answer(&self, client: &mut Client, id: i64, approve: bool) -> Result<Value> {
        client
            .call(
                "approval.answer",
                json!({
                    "approval_id": id,
                    "decision": match approve { true => "approve", false => "deny" },
                }),
            )
            .await
    }

    pub async fn rollback(&self, client: &mut Client) -> Result<Value> {
        client.call("reset", json!({"env": self.env})).await
    }
}

pub fn build(
    env: &str,
    status: &Value,
    events: &Value,
    history: &Value,
    approvals: &Value,
) -> UiModel {
    let mut model = UiModel::new();
    model.envs = tree(status);
    model.selected_session = None;
    model.transcript = transcript(history);
    model.events = event_lines(events);
    model.approvals = approval_rows(approvals);
    model.status = status_line(env, status);
    model.input_hint = "p prompt  a approve  r rollback  0-9 fold  q quit".to_string();
    model
}

fn tree(status: &Value) -> Vec<NodeInfo> {
    let rows = array(status, "nodes");
    rows.iter()
        .filter(|row| str_of(row, "parent").is_none())
        .map(|row| node(row, &rows))
        .collect()
}

fn node(row: &Value, rows: &[Value]) -> NodeInfo {
    let name = text(row, "env");
    let state = match row.get("registered") == Some(&json!(true)) {
        true => "up",
        false => "down",
    };
    let mut info = NodeInfo::new(name.clone(), text(row, "role"), state)
        .with_restarts(u64_or(row, "restarts", 0) as u32);
    if let Some(backend) = row.get("sandbox").and_then(|value| value.get("backend")) {
        info = info.with_sandbox(backend.as_str().unwrap_or_default().to_string());
    }
    let children: Vec<NodeInfo> = rows
        .iter()
        .filter(|child| str_of(child, "parent") == Some(name.as_str()))
        .map(|child| node(child, rows))
        .collect();
    info.with_children(children)
}

fn status_line(env: &str, status: &Value) -> StatusLine {
    let budget = array(status, "nodes")
        .into_iter()
        .find(|row| str_of(row, "env") == Some(env))
        .and_then(|row| row.get("budget").cloned())
        .unwrap_or(Value::Null);
    let killed = str_of(status, "killed");
    let line = match killed {
        Some(reason) => Some(format!("KILL SWITCH: {reason}")),
        None => budget_line(env, &budget),
    };
    StatusLine {
        base_pid: u64_or(status, "pid", 0) as u32,
        attached: u64_or(status, "attached", 0) as usize,
        budgets: line,
    }
}

fn budget_line(env: &str, budget: &Value) -> Option<String> {
    if !budget.is_object() {
        return None;
    }
    let limits = &budget["limits"];
    let halted = match budget["halted"].as_str() {
        Some(reason) => format!(" HALTED: {reason}"),
        None => String::new(),
    };
    Some(format!(
        "{env}: {} tokens (limit {}), {:.4} usd (limit {}), {} s (limit {}){halted}",
        budget["tokens"],
        limits["tokens"],
        budget["usd"].as_f64().unwrap_or(0.0),
        limits["usd"],
        budget["wall_s"],
        limits["wall_s"]
    ))
}

fn transcript(history: &Value) -> Vec<TranscriptItem> {
    let mut items = Vec::new();
    for row in array(history, "events") {
        let data = value(&row, "data");
        match text(&row, "kind").as_str() {
            "user/message" => items.push(TranscriptItem::message(Role::User, text(&data, "text"))),
            "assistant/message" => {
                let body = data["message"]["content"].as_str().unwrap_or_default();
                if !body.is_empty() {
                    items.push(TranscriptItem::message(Role::Assistant, body));
                }
            }
            "tool/result" => items.push(TranscriptItem::tool(
                text(&data, "name"),
                text(&data, "text"),
            )),
            _ => {}
        }
    }
    if items.len() > TRANSCRIPT_MAX {
        items.drain(..items.len() - TRANSCRIPT_MAX);
    }
    items
}

fn event_lines(events: &Value) -> Vec<EventLine> {
    array(events, "events")
        .iter()
        .map(|row| {
            EventLine::new(
                i64_or(row, "at", 0),
                text(row, "kind"),
                summary(row.get("data").unwrap_or(&Value::Null)),
            )
        })
        .collect()
}

fn approval_rows(approvals: &Value) -> Vec<Approval> {
    array(approvals, "approvals")
        .iter()
        .map(|row| {
            Approval::new(
                row.get("id").map(|id| id.to_string()).unwrap_or_default(),
                text(row, "env"),
                text(row, "reason"),
            )
        })
        .collect()
}

fn summary(data: &Value) -> String {
    let body = match data.as_str() {
        Some(text) => text.to_string(),
        None => data.to_string(),
    };
    let body = body.replace('\n', " ");
    match body.chars().count() > SUMMARY_CHARS {
        true => body.chars().take(SUMMARY_CHARS).collect(),
        false => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_tree_a_transcript_and_a_budget_line() {
        let status = json!({
            "pid": 42,
            "attached": 1,
            "killed": Value::Null,
            "nodes": [
                {"env": "root", "role": "agent", "registered": true, "restarts": 0,
                 "sandbox": {"backend": "oci"}, "parent": Value::Null,
                 "budget": {"tokens": 10, "usd": 0.5, "wall_s": 3,
                            "limits": {"tokens": 100, "usd": 1.0, "wall_s": 0, "processes": 0},
                            "halted": Value::Null}},
                {"env": "root.1", "role": "agent", "registered": false, "restarts": 1,
                 "parent": "root"}
            ],
        });
        let events = json!({"events": [{"at": 7, "kind": "turn/end", "data": {"ok": true}}]});
        let history = json!({"events": [
            {"kind": "user/message", "data": {"text": "hello"}},
            {"kind": "assistant/message", "data": {"message": {"content": "hi"}}},
            {"kind": "tool/result", "data": {"name": "bash", "text": "one\ntwo"}},
        ]});
        let approvals = json!({"approvals": [{"id": 3, "env": "root", "reason": "push out"}]});
        let model = build("root", &status, &events, &history, &approvals);
        assert_eq!(model.envs.len(), 1);
        assert_eq!(model.envs[0].children.len(), 1);
        assert_eq!(model.transcript.len(), 3);
        assert!(model.transcript[2].is_tool());
        assert_eq!(model.events.len(), 1);
        assert_eq!(model.approvals[0].id, "3");
        assert_eq!(model.status.base_pid, 42);
        assert!(model.status.budgets.unwrap().contains("10 tokens"));
    }

    #[test]
    fn the_kill_switch_replaces_the_budget_line() {
        let status = json!({"pid": 1, "attached": 0, "killed": "STOP exists", "nodes": []});
        let model = build("root", &status, &Value::Null, &Value::Null, &Value::Null);
        assert_eq!(
            model.status.budgets.as_deref(),
            Some("KILL SWITCH: STOP exists")
        );
    }
}
