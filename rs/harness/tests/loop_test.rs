mod support;

use serde_json::{json, Value};
use std::sync::Arc;
use support::{FakeBus, MemLog};
use tenon_harness::agent::Agent;
use tenon_harness::bus::{Bus, Log};
use tenon_harness::fake::{self, Say};
use tenon_harness::prompt::Prompt;
use tenon_harness::tools::{self, Tools};

struct World {
    agent: Arc<Agent>,
    log: Arc<MemLog>,
    bus: Arc<FakeBus>,
    prompt: Arc<Prompt>,
}

fn world(base_url: &str) -> World {
    let bus = Arc::new(FakeBus::default());
    let log = Arc::new(MemLog::default());
    let tools = Arc::new(Tools::new(bus.clone() as Arc<dyn Bus>));
    for row in tools::builtins(20_000) {
        tools.register(row);
    }
    let prompt = Arc::new(Prompt::new());
    prompt.builtin("/workspace", "root");
    let agent = Arc::new(Agent::new(
        bus.clone() as Arc<dyn Bus>,
        log.clone() as Arc<dyn Log>,
        support::llm(base_url),
        tools,
        prompt.clone(),
        4,
    ));
    World {
        agent,
        log,
        bus,
        prompt,
    }
}

async fn session(world: &World) -> String {
    world
        .agent
        .call("session.create", &[json!({})])
        .await
        .unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_turn_streams_chunks_and_ends_with_the_answer() {
    let server = fake::spawn(vec![Say::Text("pong".to_string())])
        .await
        .unwrap();
    let world = world(&server.base_url);
    let id = session(&world).await;
    world
        .agent
        .call(
            "session.prompt",
            &[json!({"session_id": id, "text": "say pong"})],
        )
        .await
        .unwrap();
    support::settle("turn/end", || !world.log.of("turn/end").is_empty()).await;
    let kinds = world.log.kinds();
    for wanted in [
        "session/created",
        "user/message",
        "turn/start",
        "step/start",
        "assistant/chunk",
        "assistant/message",
        "step/end",
        "turn/end",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == wanted),
            "{wanted} missing in {kinds:?}"
        );
    }
    let end = world.log.of("turn/end").pop().unwrap();
    assert_eq!(end["ok"], json!(true));
    assert_eq!(end["text"], json!("pong"));
    assert_eq!(end["session"], json!(id));
    let status = world
        .agent
        .call("session.status", &[json!({"session_id": id})])
        .await
        .unwrap();
    assert_eq!(status["turns"], json!(1));
    assert_eq!(status["running"], json!(false));
    assert_eq!(status["usage"]["total"], json!(18));
}

#[tokio::test]
async fn a_tool_call_runs_through_the_bus_and_feeds_the_next_step() {
    let server = fake::spawn(vec![
        Say::Tool("bash".to_string(), json!({"cmd": "echo tenon-ok"})),
        Say::Text("the output was tenon-ok".to_string()),
    ])
    .await
    .unwrap();
    let world = world(&server.base_url);
    world.bus.service(
        "worker",
        "bash",
        Ok(json!({"status": 0, "tail": "tenon-ok"})),
    );
    let id = session(&world).await;
    world
        .agent
        .call(
            "session.prompt",
            &[json!({"session_id": id, "text": "run it"})],
        )
        .await
        .unwrap();
    support::settle("turn/end", || !world.log.of("turn/end").is_empty()).await;
    assert_eq!(world.bus.seen("worker", "bash"), 1);
    let call = world.log.of("tool/call").pop().unwrap();
    assert_eq!(call["name"], json!("bash"));
    assert_eq!(call["arguments"]["cmd"], json!("echo tenon-ok"));
    let result = world.log.of("tool/result").pop().unwrap();
    assert_eq!(result["ok"], json!(true));
    assert!(result["text"].as_str().unwrap().contains("tenon-ok"));
    let end = world.log.of("turn/end").pop().unwrap();
    assert_eq!(end["text"], json!("the output was tenon-ok"));
    let second = &server.requests()[1];
    let roles: Vec<&str> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool"]);
}

#[tokio::test]
async fn a_pre_execute_hook_denies_the_call_with_its_own_reason() {
    let server = fake::spawn(vec![
        Say::Tool("bash".to_string(), json!({"cmd": "rm -rf /"})),
        Say::Text("it was blocked".to_string()),
    ])
    .await
    .unwrap();
    let world = world(&server.base_url);
    world.bus.hook(
        "tools/pre-execute",
        json!({"deny": "blocked by tenon guard"}),
    );
    let id = session(&world).await;
    world
        .agent
        .call(
            "session.prompt",
            &[json!({"session_id": id, "text": "clean up"})],
        )
        .await
        .unwrap();
    support::settle("turn/end", || !world.log.of("turn/end").is_empty()).await;
    assert_eq!(world.bus.seen("worker", "bash"), 0);
    let result = world.log.of("tool/result").pop().unwrap();
    assert_eq!(result["denied"], json!(true));
    assert_eq!(result["text"], json!("blocked by tenon guard"));
}

#[tokio::test]
async fn a_model_error_fails_the_turn_gracefully() {
    let server = fake::spawn(vec![Say::Status(400)]).await.unwrap();
    let world = world(&server.base_url);
    let id = session(&world).await;
    world
        .agent
        .call("session.prompt", &[json!({"session_id": id, "text": "hi"})])
        .await
        .unwrap();
    support::settle("turn/end", || !world.log.of("turn/end").is_empty()).await;
    let end = world.log.of("turn/end").pop().unwrap();
    assert_eq!(end["ok"], json!(false));
    assert!(end["error"].as_str().unwrap().contains("http 400"));
    let status = world
        .agent
        .call("session.status", &[json!({"session_id": id})])
        .await
        .unwrap();
    assert_eq!(status["running"], json!(false));
}

#[tokio::test]
async fn resume_rebuilds_the_context_from_the_log() {
    let server = fake::spawn(vec![Say::Text("resumed".to_string())])
        .await
        .unwrap();
    let world = world(&server.base_url);
    let id = "s-old".to_string();
    world.log.seed("session/created", json!({"session": id}));
    world.log.seed(
        "user/message",
        json!({"session": id, "text": "remember 41"}),
    );
    world.log.seed(
        "assistant/message",
        json!({"session": id, "message": {"role": "assistant", "content": "noted"}}),
    );
    world.log.seed(
        "tool/result",
        json!({"session": id, "id": "call_1", "text": "tenon-ok"}),
    );
    world
        .log
        .seed("turn/end", json!({"session": id, "ok": true}));
    world.log.seed(
        "user/message",
        json!({"session": "other", "text": "not mine"}),
    );
    let resumed = world
        .agent
        .call("session.resume", &[json!({"session_id": id})])
        .await
        .unwrap();
    assert_eq!(resumed["messages"], json!(3));
    assert_eq!(resumed["turns"], json!(1));
    world
        .agent
        .call(
            "session.prompt",
            &[json!({"session_id": id, "text": "what did I say"})],
        )
        .await
        .unwrap();
    support::settle("turn/end", || world.log.of("turn/end").len() == 2).await;
    let sent = &server.requests()[0]["messages"];
    let text = sent.to_string();
    assert!(text.contains("remember 41"), "{text}");
    assert!(!text.contains("not mine"), "{text}");
    let history = world
        .agent
        .call("session.history", &[json!({"session_id": id})])
        .await
        .unwrap();
    assert!(history["count"].as_i64().unwrap() >= 6);
    assert!(history["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|event| event["data"]["session"] == json!(id)));
}

#[tokio::test]
async fn the_prompt_is_assembled_from_registered_sections_in_order() {
    let server = fake::spawn(vec![Say::Text("ok".to_string())])
        .await
        .unwrap();
    let world = world(&server.base_url);
    world
        .prompt
        .call(
            "register",
            &[json!({"name": "late", "order": 500, "text": "LAST"})],
        )
        .unwrap();
    let disposer = world
        .prompt
        .call(
            "register",
            &[json!({"name": "early", "order": -500, "text": "FIRST"})],
        )
        .unwrap();
    let rendered = world.prompt.render();
    assert!(rendered.starts_with("FIRST"), "{rendered}");
    assert!(rendered.ends_with("LAST"), "{rendered}");
    assert!(rendered.contains("How to extend Tenon"));
    let names: Vec<Value> = world.prompt.list()["sections"].as_array().unwrap().clone();
    assert_eq!(names[0]["name"], json!("early"));
    assert_eq!(names.last().unwrap()["name"], json!("late"));
    let id = disposer["id"].as_u64().unwrap();
    assert_eq!(
        world
            .prompt
            .call("unregister", &[json!({"id": id})])
            .unwrap()["ok"],
        json!(true)
    );
    assert!(!world.prompt.render().contains("FIRST"));
    let session = session(&world).await;
    world
        .agent
        .call(
            "session.prompt",
            &[json!({"session_id": session, "text": "hi"})],
        )
        .await
        .unwrap();
    support::settle("turn/end", || !world.log.of("turn/end").is_empty()).await;
    let system = server.requests()[0]["messages"][0].clone();
    assert_eq!(system["role"], json!("system"));
    assert!(system["content"].as_str().unwrap().ends_with("LAST"));
}

#[tokio::test]
async fn the_tools_bus_is_a_single_authority() {
    let bus = Arc::new(FakeBus::default());
    let tools = Tools::new(bus.clone() as Arc<dyn Bus>);
    for row in tools::builtins(20_000) {
        tools.register(row);
    }
    let names: Vec<String> = tools.rows().into_iter().map(|row| row.name).collect();
    for wanted in [
        "bash",
        "view_file",
        "edit_file",
        "write_file",
        "grep",
        "glob",
        "snapshot",
    ] {
        assert!(
            names.contains(&wanted.to_string()),
            "{wanted} missing in {names:?}"
        );
    }
    let low = tools
        .call(
            "register",
            &[json!({
                "name": "bash",
                "owner": "intruder",
                "priority": -1,
                "schema": {"description": "mine"},
                "target": {"service": "other", "method": "run"},
            })],
        )
        .await
        .unwrap();
    assert_eq!(low["ok"], json!(false));
    assert_eq!(low["kept"], json!("harness"));
    assert!(bus
        .logs
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("stays with harness")));
    assert_eq!(
        tools
            .rows()
            .into_iter()
            .find(|row| row.name == "bash")
            .unwrap()
            .service,
        "worker"
    );
    let high = tools
        .call(
            "register",
            &[json!({
                "name": "bash",
                "owner": "upgrade",
                "priority": 5,
                "schema": {"description": "better bash"},
                "target": {"service": "other", "method": "run"},
            })],
        )
        .await
        .unwrap();
    assert_eq!(high["ok"], json!(true));
    let row = tools
        .rows()
        .into_iter()
        .find(|row| row.name == "bash")
        .unwrap();
    assert_eq!(row.service, "other");
    assert_eq!(row.schema["name"], json!("bash"));
    tools.execute("bash", json!({"cmd": "x"}), None).await;
    assert_eq!(bus.seen("other", "run"), 1);
    let unknown = tools.execute("nope", json!({}), None).await;
    assert!(!unknown.ok);
    assert!(unknown.text().contains("unknown tool nope"));
}

#[tokio::test]
async fn pre_execute_may_rewrite_the_arguments() {
    let bus = Arc::new(FakeBus::default());
    let tools = Tools::new(bus.clone() as Arc<dyn Bus>);
    for row in tools::builtins(20_000) {
        tools.register(row);
    }
    bus.hook(
        "tools/pre-execute",
        json!([{"name": "bash", "arguments": {"cmd": "echo safe"}, "callId": "c1"}]),
    );
    let outcome = tools
        .execute(
            "bash",
            json!({"cmd": "echo unsafe"}),
            Some("c1".to_string()),
        )
        .await;
    assert!(outcome.ok);
    let calls = bus.svcs.lock().unwrap().clone();
    assert_eq!(calls[0].2[0]["cmd"], json!("echo safe"));
    let waterfalls: Vec<String> = bus
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(event, _)| event.clone())
        .collect();
    assert_eq!(waterfalls, vec!["tools/pre-execute", "tools/post-execute"]);
}
