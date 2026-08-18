use crate::config::Config;
use crate::home::Home;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub const GUARDIAN: &str = "guardian";

#[derive(Debug, Clone)]
pub struct Spec {
    pub role: String,
    pub env: String,
    pub profile: PathBuf,
    pub sock: PathBuf,
    pub target: String,
}

pub struct Running {
    pub pid: i32,
    pub exited: Option<oneshot::Receiver<Option<i32>>>,
}

#[derive(Debug, Clone)]
pub struct Exit {
    pub env: String,
    pub generation: u64,
    pub code: Option<i32>,
}

pub fn spec(config: &Config, home: &Home, role: &str, env: &str) -> Spec {
    Spec {
        role: role.to_string(),
        env: env.to_string(),
        profile: home.profile(env),
        sock: home.sock(),
        target: config.root_env.clone(),
    }
}

pub fn spawn(
    spec: &Spec,
    config: &Config,
    home: &Home,
    release: &Path,
    generation: u64,
    exits: mpsc::UnboundedSender<Exit>,
) -> Result<Running> {
    let binary = release.join("bin/tenon_beam");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.log(&spec.env))
        .with_context(|| format!("open node log for {}", spec.env))?;
    let mut command = tokio::process::Command::new(&binary);
    command
        .arg("start")
        .current_dir(home.run())
        .env("TENON_ROLE", &spec.role)
        .env("TENON_ENV", &spec.env)
        .env("TENON_BASE_SOCK", &spec.sock)
        .env("TENON_PROFILE", &spec.profile)
        .env("TENON_GUARDIAN_TARGET", &spec.target)
        .env(
            "TENON_GUARDIAN_INTERVAL_MS",
            config.guardian.interval_ms.to_string(),
        )
        .env(
            "TENON_GUARDIAN_FAILURES",
            config.guardian.failures.to_string(),
        )
        .env("RELEASE_TMP", home.run())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(false);

    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))?;
    let pid = child.id().context("node has no pid")? as i32;
    let (tx, rx) = oneshot::channel();
    let env = spec.env.clone();
    tokio::spawn(async move {
        let code = child.wait().await.ok().and_then(|status| status.code());
        let _ = tx.send(code);
        let _ = exits.send(Exit {
            env,
            generation,
            code,
        });
    });
    Ok(Running {
        pid,
        exited: Some(rx),
    })
}

pub fn signal(pid: i32, sig: i32) {
    if pid > 0 {
        unsafe { libc::kill(pid, sig) };
    }
}

pub fn alive(pid: i32) -> bool {
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

pub async fn terminate(pid: i32, exited: Option<oneshot::Receiver<Option<i32>>>, grace: Duration) {
    signal(pid, libc::SIGTERM);
    let Some(exited) = exited else {
        wait_gone(pid, grace).await;
        signal(pid, libc::SIGKILL);
        return;
    };
    if tokio::time::timeout(grace, exited).await.is_err() {
        signal(pid, libc::SIGKILL);
        wait_gone(pid, Duration::from_secs(2)).await;
    }
}

async fn wait_gone(pid: i32, limit: Duration) {
    let deadline = tokio::time::Instant::now() + limit;
    while alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
