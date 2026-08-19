pub mod agent;
pub mod api;
pub mod bus;
pub mod config;
pub mod fake;
pub mod llm;
pub mod manage;
pub mod prompt;
pub mod tools;
pub mod wire;

use crate::agent::Agent;
use crate::api::{Api, ApiGate, BaseLog};
use crate::bus::{Bus, Log};
use crate::config::Settings;
use crate::manage::Manage;
use crate::prompt::Prompt;
use crate::tools::Tools;
use crate::wire::{method, Router};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

pub const ROLE: &str = "harness";
pub const CONFIG_ENV: &str = "TENON_HARNESS_CONFIG";

pub fn run(args: &[String]) -> i32 {
    let env = env_name(args);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("tenon harness: no runtime: {error}");
            return 2;
        }
    };
    match runtime.block_on(serve(env)) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("tenon harness: {error:#}");
            2
        }
    }
}

fn env_name(args: &[String]) -> String {
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        if arg == "--env" {
            if let Some(name) = items.next() {
                return name.clone();
            }
        }
        if let Some(name) = arg.strip_prefix("--env=") {
            return name.to_string();
        }
    }
    std::env::var("TENON_ENV").unwrap_or_else(|_| "root".to_string())
}

async fn serve(env: String) -> Result<()> {
    let settings = Settings::from_env(CONFIG_ENV);
    let sock =
        PathBuf::from(std::env::var("TENON_BASE_SOCK").context("TENON_BASE_SOCK is not set")?);
    let api = Arc::new(Api::new(sock, env.clone()));
    let gateway = std::env::var("TENON_GATEWAY").ok();
    let (reader, writer) = wire::ends(gateway.as_deref())
        .await
        .context("open the wire")?;
    let wire = wire::open(writer);
    let bus: Arc<dyn Bus> = wire.clone();
    let log: Arc<dyn Log> = Arc::new(BaseLog::new(api.clone()));
    let llm = Arc::new(llm::Client::new(settings.llm.clone()));
    let tools = Arc::new(Tools::new(bus.clone()));
    tools.set_gate(Arc::new(ApiGate::new(api.clone())), &settings.gated_tools);
    let prompt = Arc::new(Prompt::new());
    let manage = Arc::new(Manage::new(api.clone(), bus.clone()));
    let agent = Arc::new(Agent::new(
        bus.clone(),
        log.clone(),
        llm.clone(),
        tools.clone(),
        prompt.clone(),
        settings.max_steps,
    ));
    let router = router(&agent, &tools, &prompt, &manage, &llm);
    let ready = {
        let tools = tools.clone();
        let prompt = prompt.clone();
        let llm = llm.clone();
        let api = api.clone();
        let env = env.clone();
        let timeout = settings.tool_timeout_ms;
        move |_wire: Arc<wire::Wire>, _config: Value| {
            for row in tools::builtins(timeout) {
                tools.register(row);
            }
            prompt.builtin("/workspace", &env);
            let model = llm.describe();
            tokio::spawn(async move {
                let _ = api
                    .env_call(
                        "events.append",
                        json!({
                            "kind": "harness/ready",
                            "data": {"pid": std::process::id(), "llm": model},
                        }),
                    )
                    .await;
            });
        }
    };
    eprintln!(
        "tenon harness: env {env}, model {}, wire {}",
        llm.model(),
        gateway.unwrap_or_else(|| "fd 3/4".to_string())
    );
    wire::serve(wire, reader, router, ready).await;
    Ok(())
}

fn router(
    agent: &Arc<Agent>,
    tools: &Arc<Tools>,
    prompt: &Arc<Prompt>,
    manage: &Arc<Manage>,
    llm: &Arc<llm::Client>,
) -> Router {
    let mut router = Router::default();
    for name in [
        "session.create",
        "session.prompt",
        "session.status",
        "session.history",
        "session.resume",
        "sessions",
    ] {
        let agent = agent.clone();
        router.service(
            "loop",
            vec![(
                name,
                method(move |args| {
                    let agent = agent.clone();
                    async move { agent.call(name, &args).await }
                }),
            )],
        );
    }
    for name in ["register", "unregister", "list", "execute"] {
        let tools = tools.clone();
        router.service(
            "tools",
            vec![(
                name,
                method(move |args| {
                    let tools = tools.clone();
                    async move { tools.call(name, &args).await }
                }),
            )],
        );
    }
    for name in ["register", "unregister", "list", "render"] {
        let prompt = prompt.clone();
        router.service(
            "prompt",
            vec![(
                name,
                method(move |args| {
                    let prompt = prompt.clone();
                    async move { prompt.call(name, &args) }
                }),
            )],
        );
    }
    for name in [
        "plugin.tool",
        "plugin.list",
        "plugin.mount",
        "plugin.unmount",
        "plugin.restart",
        "config.tool",
        "config.get",
        "config.patch",
        "snapshot.tool",
        "snapshot.list",
        "snapshot.commit",
        "snapshot.restore",
        "runtime.spawn",
        "approval.request",
    ] {
        let manage = manage.clone();
        router.service(
            tools::MANAGE,
            vec![(
                name,
                method(move |args| {
                    let manage = manage.clone();
                    async move { manage.call(name, &args).await }
                }),
            )],
        );
    }
    let client = llm.clone();
    router.service(
        "llm",
        vec![
            (
                "chat",
                method(move |args| {
                    let client = client.clone();
                    async move { chat(&client, args).await }
                }),
            ),
            ("models", {
                let client = llm.clone();
                method(move |_args| {
                    let client = client.clone();
                    async move { client.models().await }
                })
            }),
        ],
    );
    router
}

async fn chat(client: &llm::Client, args: Vec<Value>) -> Result<Value, String> {
    let params = args.first().cloned().unwrap_or(json!({}));
    let messages = params
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if messages.is_empty() {
        return Err("llm.chat needs messages".to_string());
    }
    let tools = params
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let stream = params
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let request = client.request(messages, tools, stream);
    client
        .chat(&request, |_delta| {})
        .await
        .map(|reply| reply.json())
}
