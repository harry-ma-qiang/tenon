use crate::bench;
use crate::config::Benchmark;
use crate::home::Home;
use crate::peer::Peer;
use crate::rpc::Cmd;
use crate::upgrade::{phase, CANARY, PROMOTED, ROLLED_BACK};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::Instance;
use tokio::sync::{mpsc, oneshot};

pub type Answer = Result<Value, String>;

/// Everything the change protocol needs, taken off the actor once. The driver
/// runs `snapshot -> canary -> verify -> promote | rollback` as ordinary wire
/// requests and file work, and comes back through `Cmd`s for the few steps
/// only base may perform.
pub struct Drive {
    pub id: i64,
    pub env: String,
    pub target: String,
    pub artifact: Value,
    pub peer: Peer,
    pub cmds: mpsc::UnboundedSender<Cmd>,
    pub home: Home,
    pub release: PathBuf,
    pub instance: Option<Arc<dyn Instance>>,
    pub gateway: String,
    pub bench: Benchmark,
    pub timeout: Duration,
    pub worker_timeout: Duration,
}

impl Drive {
    pub fn phase(&self, status: &str, step: Value) {
        let _ = self.cmds.send(Cmd::UpgradePhase {
            id: self.id,
            status: status.to_string(),
            reason: None,
            step,
        });
    }

    pub fn done(&self, status: &str, reason: Option<String>, step: Value) {
        let _ = self.cmds.send(Cmd::UpgradePhase {
            id: self.id,
            status: status.to_string(),
            reason,
            step,
        });
    }

    pub async fn ask<F>(&self, build: F) -> Answer
    where
        F: FnOnce(oneshot::Sender<Answer>) -> Cmd,
    {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(build(tx))
            .map_err(|_| "base is gone".to_string())?;
        rx.await.map_err(|_| "base is gone".to_string())?
    }

    /// A `plugin` frame to that env's node: mount, unmount, list, or the owner
    /// of a service name.
    pub async fn plugin(&self, op: &str, body: Value) -> Answer {
        let mut params = json!({"op": op});
        if let (Some(target), Some(rows)) = (params.as_object_mut(), body.as_object()) {
            for (key, value) in rows {
                target.insert(key.clone(), value.clone());
            }
        }
        let answer = self
            .peer
            .request("plugin", params, self.timeout.max(Duration::from_secs(60)))
            .await?;
        match answer["ok"] == json!(true) {
            true => Ok(answer),
            false => Err(answer["error"]
                .as_str()
                .unwrap_or("plugin failed")
                .to_string()),
        }
    }

    pub async fn svc(&self, name: &str, method: &str, args: Value) -> Answer {
        self.svc_args(name, method, vec![args]).await
    }

    /// A `svc` call with the exact argument list, which the conformance of a
    /// candidate needs: `selfcheck` takes no arguments at all.
    pub async fn svc_args(&self, name: &str, method: &str, args: Vec<Value>) -> Answer {
        self.peer
            .request(
                "svc",
                json!({"name": name, "method": method, "args": args}),
                self.timeout.max(Duration::from_secs(60)),
            )
            .await
    }

    /// The fiber id currently providing a service name, or `None` when nothing
    /// provides it: what tells the protocol which fiber the promotion has to
    /// unmount, since a socket fiber's id is the gateway's, not the plugin's.
    pub async fn owner(&self, service: &str) -> Option<String> {
        let answer = self.plugin("owner", json!({"name": service})).await.ok()?;
        answer["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    }

    pub fn text(&self, key: &str) -> String {
        crate::params::text(&self.artifact, key)
    }
}

/// The protocol of RFC section 10, executed only by base. One proposal is one
/// pass through it; every phase is recorded before the next one starts.
pub async fn drive(drive: Drive) {
    match run(&drive).await {
        Ok(data) => drive.done(PROMOTED, None, phase("promote", true, data)),
        Err(reason) => {
            let restored = rollback(&drive).await;
            drive.done(
                ROLLED_BACK,
                Some(reason.clone()),
                phase(
                    "rollback",
                    true,
                    json!({"reason": reason, "restored": restored}),
                ),
            );
        }
    }
}

async fn run(drive: &Drive) -> Answer {
    let snapshot = snapshot(drive).await?;
    drive.phase("snapshot", phase("snapshot", true, snapshot.clone()));
    let baseline = baseline(drive).await;
    drive.phase("snapshot", phase("baseline", true, baseline.clone()));
    let canary = canary(drive).await?;
    drive.phase(CANARY, phase("canary", true, canary));
    let verify = verify(drive).await?;
    drive.phase("verify", phase("verify", true, verify));
    promote(drive, &snapshot).await
}

/// The LKG-side number of the promotion gate, measured now rather than
/// remembered from an older machine: the baseline runs before the canary is
/// mounted, so the two passes differ only in the artifact under test.
async fn baseline(drive: &Drive) -> Value {
    let score = bench::run(drive).await;
    let label = bench::label(&drive.bench);
    let _ = drive
        .ask(|reply| Cmd::UpgradeBench {
            env: drive.env.clone(),
            label: label.clone(),
            id: drive.id,
            row: score.row(),
            lkg: true,
            reply,
        })
        .await;
    score.json()
}

async fn snapshot(drive: &Drive) -> Answer {
    match drive.target.as_str() {
        "plugin" => crate::candidate::plugin_snapshot(drive).await,
        "worker" => crate::candidate::worker_snapshot(drive).await,
        "kernel" => crate::candidate::kernel_snapshot(drive),
        _ => config_snapshot(drive),
    }
}

async fn canary(drive: &Drive) -> Answer {
    match drive.target.as_str() {
        "plugin" => crate::candidate::plugin_canary(drive).await,
        "worker" => crate::candidate::worker_canary(drive).await,
        "kernel" => crate::candidate::kernel_canary(drive).await,
        _ => config_canary(drive),
    }
}

/// Hard rules first, then the benchmark gate. The hard rules are base's own
/// (`Cmd::Guard` is what refuses a prompt for a halted env or a killed base),
/// so a budget breach or the kill switch stops a promotion the same way it
/// stops a turn.
async fn verify(drive: &Drive) -> Answer {
    drive
        .ask(|reply| Cmd::Guard {
            env: drive.env.clone(),
            reply,
        })
        .await
        .map_err(|error| format!("hard rules: {error}"))?;
    let score = bench::run(drive).await;
    let lkg = drive
        .ask(|reply| Cmd::UpgradeBench {
            env: drive.env.clone(),
            label: bench::label(&drive.bench),
            id: drive.id,
            row: score.row(),
            lkg: false,
            reply,
        })
        .await?;
    let pair = lkg["lkg"].as_object().map(|row| {
        (
            row["success_rate"].as_f64().unwrap_or(0.0),
            row["cost"].as_i64().unwrap_or(0),
        )
    });
    bench::compare(&score, pair, drive.bench.cost_tolerance)
}

async fn promote(drive: &Drive, snapshot: &Value) -> Answer {
    match drive.target.as_str() {
        "plugin" => crate::candidate::plugin_promote(drive, snapshot).await,
        "worker" => crate::candidate::worker_promote(drive, snapshot).await,
        "kernel" => crate::candidate::kernel_promote(drive).await,
        _ => config_promote(drive).await,
    }
}

/// Destroying the canary and restoring the snapshot, best effort: a rollback
/// that fails half way still has to report the reason it started for.
async fn rollback(drive: &Drive) -> Value {
    match drive.target.as_str() {
        "plugin" => crate::candidate::plugin_rollback(drive).await,
        "worker" => crate::candidate::worker_rollback(drive).await,
        "kernel" => json!({"canary": "none"}),
        _ => config_rollback(drive).await,
    }
}

fn config_snapshot(drive: &Drive) -> Answer {
    let from = drive.home.harness_file(&drive.env);
    let dir = drive.home.config_snapshots(&drive.env);
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let into = dir.join(format!("upgrade-{}.yml", drive.id));
    std::fs::copy(&from, &into).map_err(|error| format!("snapshot {}: {error}", from.display()))?;
    Ok(json!({"target": "config", "snapshot": into, "from": from}))
}

fn config_canary(drive: &Drive) -> Answer {
    match drive.artifact.get("patch") {
        Some(patch) if patch.is_object() => Ok(json!({"patch": patch, "live": false})),
        _ => Err("an upgrade of the config needs artifact.patch as an object".to_string()),
    }
}

async fn config_promote(drive: &Drive) -> Answer {
    let patch = drive.artifact.get("patch").cloned().unwrap_or(json!({}));
    let env = drive.env.clone();
    drive
        .ask(|reply| Cmd::ConfigPatch {
            env,
            target: "env".to_string(),
            patch,
            approved: true,
            reply,
        })
        .await
}

async fn config_rollback(drive: &Drive) -> Value {
    let into = drive.home.harness_file(&drive.env);
    let from = drive
        .home
        .config_snapshots(&drive.env)
        .join(format!("upgrade-{}.yml", drive.id));
    if from.is_file() {
        let _ = std::fs::copy(&from, &into);
    }
    let _ = drive.peer.request("reload", json!({}), drive.timeout).await;
    json!({"restored": from})
}
