use tenon_ui::{Approval, EventLine, NodeInfo, Role, StatusLine, TranscriptItem, UiModel};

pub fn fixture_model() -> UiModel {
    let mut model = UiModel::new();
    model.envs = vec![
        NodeInfo::new("root", "agent", "up")
            .with_restarts(0)
            .with_children(vec![
                NodeInfo::new("root.1", "agent", "up").with_sandbox("oci"),
                NodeInfo::new("root.2", "agent", "restarting").with_restarts(3),
            ]),
        NodeInfo::new("side", "agent", "down").with_sandbox("landlock"),
    ];
    model.selected_session = Some("root".to_string());
    model.transcript = vec![
        TranscriptItem::message(Role::User, "please run the build and fix any failures"),
        TranscriptItem::message(
            Role::Assistant,
            "sure, starting with a clean build to see the current state",
        ),
        TranscriptItem::tool(
            "bash",
            "cargo build\n   Compiling tenon-ui\n    Finished dev",
        ),
        TranscriptItem::message(Role::Assistant, "build is clean, running clippy next"),
        TranscriptItem::tool(
            "bash",
            "cargo clippy --all-targets -- -D warnings\nno warnings",
        ),
    ];
    model.expanded.insert(2);
    model.events = vec![
        EventLine::new(1001, "session/created", "root"),
        EventLine::new(
            1002,
            "user/message",
            "please run the build and fix any failures",
        ),
        EventLine::new(1003, "turn/start", "turn 1"),
        EventLine::new(1004, "tool/call", "bash cargo build"),
        EventLine::new(1005, "tool/result", "ok"),
        EventLine::new(1006, "turn/end", "turn 1 ok"),
    ];
    model.approvals = vec![Approval::new(
        "ap-1",
        "root",
        "publish workspace to the host",
    )];
    model.status = StatusLine {
        base_pid: 4242,
        attached: 2,
        budgets: Some("tokens=12000/50000 wall=90s/600s".to_string()),
    };
    model.input_hint =
        "type a task, p to prompt, a to approve, r to rollback, q to quit".to_string();
    model
}
