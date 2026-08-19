use crate::frame;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

pub struct Handle {
    stop: Option<oneshot::Sender<()>>,
    service: String,
}

impl Handle {
    pub fn service(&self) -> &str {
        &self.service
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Mounts a child env as an external fiber in its parent's kernel tree: base
/// dials the parent's gateway, speaks the plugin wire, and provides the service
/// `env:<child>` on the child's behalf. Base opens the connection rather than
/// the child's node because the child's `Link` speaks base frames, not wire
/// frames; the fiber is a child of the parent's gateway fiber either way, so
/// the parent's `tree` shows the lineage and dropping this handle unmounts it.
pub fn mount(gateway: PathBuf, env: String, cmds: mpsc::UnboundedSender<Cmd>) -> Handle {
    let (stop, stopped) = oneshot::channel();
    let service = format!("env:{env}");
    let name = service.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = stopped => {}
            outcome = serve(gateway, env.clone(), name, cmds) => {
                if let Err(error) = outcome {
                    eprintln!("tenon base: env fiber for {env} ended: {error}");
                }
            }
        }
    });
    Handle {
        stop: Some(stop),
        service,
    }
}

async fn serve(
    gateway: PathBuf,
    env: String,
    service: String,
    cmds: mpsc::UnboundedSender<Cmd>,
) -> Result<(), String> {
    let mut stream = UnixStream::connect(&gateway)
        .await
        .map_err(|error| format!("connect {}: {error}", gateway.display()))?;
    write(&mut stream, &json!({"t": "hello", "inject": []})).await?;
    loop {
        let Some(request) = frame::read(&mut stream)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(());
        };
        match frame::method(&request) {
            Some("load") => {
                let req = crate::params::value(&request, "req");
                write(&mut stream, &frame::rep_req(req, Ok(json!("ok")))).await?;
                write(&mut stream, &json!({"t": "provide", "name": service})).await?;
            }
            Some("svc") => {
                let req = crate::params::value(&request, "req");
                let method = crate::params::text(&request, "method");
                let answer = call(&env, &method, &cmds).await;
                write(&mut stream, &frame::rep_req(req, answer)).await?;
            }
            Some("unload") => return Ok(()),
            _ => {}
        }
    }
}

async fn call(env: &str, method: &str, cmds: &mpsc::UnboundedSender<Cmd>) -> Result<Value, String> {
    let (tx, rx) = oneshot::channel();
    let cmd = match method {
        "status" => Cmd::EnvStatus {
            env: env.to_string(),
            reply: tx,
        },
        "stop" => Cmd::RuntimeStop {
            env: env.to_string(),
            reply: tx,
        },
        other => return Err(format!("unknown method {other}")),
    };
    cmds.send(cmd).map_err(|_| "base_gone".to_string())?;
    rx.await.map_err(|_| "base_gone".to_string())?
}

async fn write(stream: &mut UnixStream, body: &Value) -> Result<(), String> {
    frame::write(stream, body)
        .await
        .map_err(|error| error.to_string())
}
