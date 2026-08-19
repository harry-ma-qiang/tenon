use crate::base::Base;
use crate::node::GUARDIAN;
use crate::state::{Node, WorkerState};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::{Endpoint, Instance, Spec};
use tokio::sync::oneshot;

fn sandbox_env_passthrough() -> Vec<String> {
    std::env::var("TENON_SANDBOX_ENV")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

pub fn endpoint_repr(endpoint: &Endpoint) -> Value {
    match endpoint {
        Endpoint::Direct => json!("direct"),
        Endpoint::Uds(path) => json!(format!("unix:{}", path.display())),
        Endpoint::Tcp(host, port) => json!(format!("tcp:{host}:{port}")),
    }
}

impl Base {
    pub fn enter_sandbox(
        &mut self,
        role: &str,
        env: &str,
        previous: Option<&Node>,
        ram_mb: u64,
    ) -> Result<Option<Arc<dyn Instance>>, String> {
        if role == GUARDIAN {
            return Ok(None);
        }
        if let Some(old) = previous.and_then(|node| node.sandbox.clone()) {
            let _ = old.destroy();
        }
        let policy = tenon_sandbox::Policy {
            ram_mb,
            ..Default::default()
        };
        let spec = Spec {
            env: env.to_string(),
            image: std::env::var("TENON_SANDBOX_IMAGE").ok(),
            binary: std::env::current_exe().ok(),
            workspace: self.home.workspace_dir(env),
            gateway: Some(self.gateway_address(env)),
            env_passthrough: sandbox_env_passthrough(),
            policy,
            caps: vec![],
            home_hash: self.home.hash(),
            base_pid: std::process::id() as i32,
            images: Some(self.home.images_dir()),
        };
        self.sandbox
            .spawn(&spec)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    pub fn sandbox_exec(
        &mut self,
        env: String,
        cmd: String,
        args: Vec<String>,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<Value, String>>,
    ) {
        let Some(instance) = self.nodes.get(&env).and_then(|node| node.sandbox.clone()) else {
            let _ = reply.send(Err(format!("env {env} has no sandbox instance")));
            return;
        };
        tokio::task::spawn_blocking(move || {
            let outcome = instance.exec(&cmd, &args, Duration::from_millis(timeout_ms.max(1)));
            let result = outcome
                .map(|outcome| {
                    json!({
                        "status": outcome.status,
                        "stdout": String::from_utf8_lossy(&outcome.stdout),
                        "stderr": String::from_utf8_lossy(&outcome.stderr),
                        "timed_out": outcome.timed_out,
                    })
                })
                .map_err(|error| error.to_string());
            let _ = reply.send(result);
        });
    }

    pub fn sandbox_destroy(&mut self, env: &str, reply: oneshot::Sender<Result<Value, String>>) {
        let Some(node) = self.nodes.get_mut(env) else {
            let _ = reply.send(Err(format!("unknown env {env}")));
            return;
        };
        let Some(instance) = node.sandbox.take() else {
            let _ = reply.send(Err(format!("env {env} has no sandbox instance")));
            return;
        };
        node.worker = WorkerState::Off;
        node.ticker = None;
        self.emit("sandbox.destroy", Some(env), json!({"id": instance.id()}));
        tokio::task::spawn_blocking(move || {
            let _ = instance.destroy();
        });
        let _ = reply.send(Ok(json!({"ok": true})));
    }
}
