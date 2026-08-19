use crate::client::Client;
use crate::home::Home;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// `tenon run "task"`: create a session in that env's harness, prompt it, and
/// stream the session log until the turn ends. The event log is the only thing
/// this reads — the same rows `tenon attach` shows and a replay would fold.
pub async fn task(
    home: Option<PathBuf>,
    env: Option<String>,
    text: String,
    timeout: Duration,
) -> Result<i32> {
    let home = Home::resolve(home)?;
    let mut events = Client::connect(&home.sock()).await?;
    let mut calls = Client::connect(&home.sock()).await?;
    let scope = match &env {
        Some(env) => json!({ "env": env }),
        None => json!({}),
    };
    events.call("subscribe", scope.clone()).await?;
    let created = calls.call("session.create", scope.clone()).await?;
    let Some(session) = crate::params::str_of(&created, "session_id") else {
        bail!("the harness answered session.create with {created}");
    };
    let session = session.to_string();
    let mut params = scope.clone();
    params["session_id"] = json!(session);
    params["text"] = json!(text);
    calls.call("session.prompt", params).await?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut streamed = false;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            eprintln!("tenon run: no turn/end within {} s", timeout.as_secs());
            return Ok(1);
        }
        let event = match tokio::time::timeout(left, events.event()).await {
            Err(_) => continue,
            Ok(Ok(Some(event))) => event,
            Ok(Ok(None)) => bail!("base closed the connection"),
            Ok(Err(error)) => return Err(error),
        };
        match event["kind"].as_str().unwrap_or_default() {
            "budget.exceeded" | "kill.switch" => {
                eprintln!(
                    "tenon run: halted: {}",
                    event["data"]["reason"]
                        .as_str()
                        .unwrap_or("no reason given")
                );
                return Ok(1);
            }
            _ => {}
        }
        if event["data"]["session"] != json!(session) {
            continue;
        }
        match event["kind"].as_str().unwrap_or_default() {
            "assistant/chunk" => {
                streamed = true;
                print!("{}", event["data"]["text"].as_str().unwrap_or_default());
                let _ = std::io::stdout().flush();
            }
            "tool/call" => eprintln!(
                "\ntenon run: tool {} {}",
                event["data"]["name"].as_str().unwrap_or("?"),
                event["data"]["arguments"]
            ),
            "tool/result" => eprintln!(
                "tenon run: tool {} {}",
                event["data"]["name"].as_str().unwrap_or("?"),
                match event["data"]["denied"] == json!(true) {
                    true => "denied",
                    false => "ok",
                }
            ),
            "turn/end" => return Ok(finish(&event["data"], &session, streamed)),
            _ => {}
        }
    }
}

/// The answer is printed once: streamed while it arrives, or in one piece
/// from `turn/end` when the model or a hook produced it without a stream.
fn finish(data: &Value, session: &str, streamed: bool) -> i32 {
    let text = data["text"].as_str().unwrap_or_default();
    println!();
    if data["ok"] == json!(true) {
        if !streamed && !text.is_empty() {
            println!("{text}");
        }
        eprintln!("tenon run: session {session} ok, usage {}", data["usage"]);
        return 0;
    }
    eprintln!("tenon run: turn failed: {}", data["error"]);
    1
}
