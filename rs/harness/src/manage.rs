use crate::api::Api;
use crate::bus::{Answer, Bus};
use serde_json::{json, Value};
use std::sync::Arc;

/// The management tools behind the `manage` service (RFC section 6): the
/// agent's hands on its own environment. Everything that touches the host goes
/// through base's front door; everything inside the sandbox goes through the
/// worker. A failure is a reason string, which the loop hands back to the model
/// as the tool result rather than failing the turn.
pub struct Manage {
    api: Arc<Api>,
    bus: Arc<dyn Bus>,
}

impl Manage {
    pub fn new(api: Arc<Api>, bus: Arc<dyn Bus>) -> Self {
        Self { api, bus }
    }

    pub async fn call(&self, method: &str, args: &[Value]) -> Answer {
        let params = args.first().cloned().unwrap_or(json!({}));
        match method {
            "plugin.tool" => self.plugin(&op(&params, "list"), &params).await,
            "plugin.list" => self.plugin("list", &params).await,
            "plugin.mount" => self.plugin("mount", &params).await,
            "plugin.unmount" => self.plugin("unmount", &params).await,
            "plugin.restart" => self.plugin("restart", &params).await,
            "config.tool" => self.config(&op(&params, "get"), &params).await,
            "config.get" => self.config("get", &params).await,
            "config.patch" => self.config("patch", &params).await,
            "snapshot.tool" => self.snapshot(&op(&params, "list"), &params).await,
            "snapshot.list" => self.snapshot("list", &params).await,
            "snapshot.commit" => self.snapshot("commit", &params).await,
            "snapshot.restore" => self.snapshot("restore", &params).await,
            "runtime.spawn" => {
                let overrides = params.get("overrides").cloned().unwrap_or(json!({}));
                let parent = self.api.env().to_string();
                self.api
                    .env_call(
                        "runtime.spawn",
                        json!({"overrides": overrides, "parent": parent}),
                    )
                    .await
            }
            "approval.request" => {
                let text = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified");
                self.api
                    .env_call("approval.request", json!({"reason": text}))
                    .await
            }
            other => Err(format!("unknown method {other}")),
        }
    }

    async fn plugin(&self, op: &str, params: &Value) -> Answer {
        let mut body = json!({"op": op});
        // `id` is the frame's own correlation id on every hop of this call, so
        // the fiber's id travels as `plugin_id` from here to the node.
        if let Some(value) = params.get("id").or_else(|| params.get("plugin_id")) {
            body["plugin_id"] = value.clone();
        }
        for key in ["spec", "name", "config"] {
            if let Some(value) = params.get(key) {
                body[key] = value.clone();
            }
        }
        self.api.env_call("plugin", body).await
    }

    async fn config(&self, op: &str, params: &Value) -> Answer {
        match op {
            "get" => self.api.env_call("config.get", json!({})).await,
            "patch" => {
                let patch = params.get("patch").cloned().unwrap_or(json!({}));
                if !patch.is_object() {
                    return Err("config.patch needs a patch object".to_string());
                }
                self.api
                    .env_call("config.patch", json!({"patch": patch}))
                    .await
            }
            other => Err(format!("unknown config op {other}")),
        }
    }

    async fn snapshot(&self, op: &str, params: &Value) -> Answer {
        match op {
            "list" => self.api.env_call("snap.list", json!({})).await,
            "commit" => {
                let label = params.get("label").cloned().unwrap_or(Value::Null);
                self.bus
                    .svc("worker", "snap.commit", vec![json!({"label": label})])
                    .await
            }
            "restore" => {
                let Some(reference) = params.get("ref").cloned() else {
                    return Err("snapshot restore needs a ref".to_string());
                };
                self.bus
                    .svc("worker", "snap.restore", vec![json!({"ref": reference})])
                    .await
            }
            other => Err(format!("unknown snapshot op {other}")),
        }
    }
}

fn op(params: &Value, fallback: &str) -> String {
    params
        .get("op")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}
