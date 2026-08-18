mod support;

use serde_json::json;
use tenon_harness::fake::{self, Say};

#[tokio::test]
async fn streams_text_deltas_and_reports_usage() {
    let server = fake::spawn(vec![Say::Text("pong from the fake model".to_string())])
        .await
        .unwrap();
    let client = support::llm(&server.base_url);
    let request = client.request(vec![json!({"role": "user", "content": "hi"})], vec![], true);
    let mut seen = String::new();
    let reply = client
        .chat(&request, |delta| seen.push_str(delta))
        .await
        .unwrap();
    assert_eq!(reply.content, "pong from the fake model");
    assert_eq!(seen, reply.content);
    assert_eq!(reply.finish, "stop");
    assert_eq!(reply.usage.total, 18);
    assert_eq!(server.requests()[0]["stream"], json!(true));
}

#[tokio::test]
async fn reassembles_a_tool_call_split_across_frames() {
    let server = fake::spawn(vec![Say::Tool(
        "bash".to_string(),
        json!({"cmd": "echo tenon-ok"}),
    )])
    .await
    .unwrap();
    let client = support::llm(&server.base_url);
    let tools = vec![json!({"type": "function", "function": {"name": "bash"}})];
    let request = client.request(
        vec![json!({"role": "user", "content": "run it"})],
        tools,
        true,
    );
    let reply = client.chat(&request, |_delta| {}).await.unwrap();
    assert_eq!(reply.finish, "tool_calls");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0]["function"]["name"], json!("bash"));
    let arguments = reply.tool_calls[0]["function"]["arguments"]
        .as_str()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(arguments).unwrap(),
        json!({"cmd": "echo tenon-ok"})
    );
    assert_eq!(reply.message()["tool_calls"][0]["id"], json!("call_fake_1"));
}

#[tokio::test]
async fn retries_a_429_and_a_500_then_succeeds() {
    let server = fake::spawn(vec![
        Say::Status(429),
        Say::Status(503),
        Say::Text("recovered".to_string()),
    ])
    .await
    .unwrap();
    let client = support::llm(&server.base_url);
    let request = client.request(vec![json!({"role": "user", "content": "hi"})], vec![], true);
    let reply = client.chat(&request, |_delta| {}).await.unwrap();
    assert_eq!(reply.content, "recovered");
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn gives_up_with_the_reason_when_every_attempt_fails() {
    let server = fake::spawn(vec![Say::Status(500), Say::Status(500), Say::Status(500)])
        .await
        .unwrap();
    let client = support::llm(&server.base_url);
    let request = client.request(vec![json!({"role": "user", "content": "hi"})], vec![], true);
    let error = client.chat(&request, |_delta| {}).await.unwrap_err();
    assert!(error.contains("http 500"), "{error}");
    assert!(error.contains("3 attempts"), "{error}");
}

#[tokio::test]
async fn a_400_is_not_retried() {
    let server = fake::spawn(vec![Say::Status(400), Say::Text("never".to_string())])
        .await
        .unwrap();
    let client = support::llm(&server.base_url);
    let request = client.request(
        vec![json!({"role": "user", "content": "hi"})],
        vec![],
        false,
    );
    let error = client.chat(&request, |_delta| {}).await.unwrap_err();
    assert!(error.contains("http 400"), "{error}");
    assert_eq!(server.requests().len(), 1);
}
