pub mod base;
pub mod client;
pub mod config;
pub mod frame;
pub mod home;
pub mod node;
pub mod peer;
pub mod release;
pub mod server;

use crate::base::Cmd;
use crate::client::Client;
use crate::config::Config;
use crate::home::Home;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tenon_storage::Store;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

const READY_POLL_MS: u64 = 50;
const DAEMON_WAIT_MS: u64 = 60_000;

pub struct StartOpts {
    pub home: Option<PathBuf>,
    pub release_dir: Option<PathBuf>,
    pub foreground: bool,
    pub exit_on_detach: bool,
    pub payload: Option<&'static [u8]>,
    pub version: &'static str,
}

pub async fn start(opts: StartOpts) -> Result<i32> {
    if opts.foreground {
        foreground(opts).await
    } else {
        daemonize(&opts)
    }
}

pub async fn foreground(opts: StartOpts) -> Result<i32> {
    let home = Home::resolve(opts.home)?;
    home.scaffold()?;
    let config = Config::load(&home.config_file())?;
    home.prepare(&config.root_env)?;
    let release = release::resolve(
        &home,
        opts.release_dir.as_deref(),
        opts.payload,
        opts.version,
    )?;
    let store = Store::open(&home.state_file())?;
    let sandbox = tenon_sandbox::backend(&config.sandbox)?;
    let sock = home.sock();
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(home.ready_file());
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;

    let (cmds, cmd_rx) = mpsc::unbounded_channel();
    let (exits, exit_rx) = mpsc::unbounded_channel();
    let state = base::Base::new(
        home.clone(),
        config.clone(),
        store,
        release.clone(),
        sandbox,
        opts.exit_on_detach,
        exits,
    );
    tokio::spawn(server::serve(
        listener,
        cmds.clone(),
        server::Opts {
            root_env: config.root_env.clone(),
            timeout: Duration::from_millis(config.request_timeout_ms),
        },
    ));
    let actor = tokio::spawn(state.run(cmd_rx, exit_rx));

    let (tx, rx) = oneshot::channel();
    cmds.send(Cmd::Boot { reply: tx })
        .map_err(|_| anyhow::anyhow!("base actor gone"))?;
    rx.await?.map_err(|error| anyhow::anyhow!(error))?;

    if !ready(&cmds, Duration::from_millis(config.boot_timeout_ms)).await {
        let (tx, rx) = oneshot::channel();
        let _ = cmds.send(Cmd::Stop { reply: tx });
        let _ = rx.await;
        let _ = actor.await;
        bail!(
            "the nodes did not register within {} ms",
            config.boot_timeout_ms
        );
    }
    std::fs::write(home.ready_file(), std::process::id().to_string())?;
    println!(
        "tenon: base ready, pid {}, home {}, release {}",
        std::process::id(),
        home.root.display(),
        release.display()
    );
    tokio::spawn(signals(cmds.clone()));
    Ok(actor.await.unwrap_or(1))
}

pub async fn attach(home: Option<PathBuf>, env: Option<String>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let mut client = Client::connect(&home.sock()).await?;
    let status = client.call("status", json!({})).await?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    let params = env.map(|env| json!({ "env": env })).unwrap_or(json!({}));
    let subscribed = client.call("subscribe", params).await?;
    println!("tenon: attached from event {}", subscribed["last_event"]);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(0),
            event = client.event() => match event? {
                None => return Ok(0),
                Some(event) => println!("{}", line(&event)),
            },
        }
    }
}

pub async fn rpc(home: Option<PathBuf>, method: &str, params: Value) -> Result<i32> {
    let home = Home::resolve(home)?;
    let mut client = Client::connect(&home.sock()).await?;
    let result = client.call(method, params).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(0)
}

fn line(event: &Value) -> String {
    format!(
        "{} {} {} {}",
        event["at"].as_i64().unwrap_or(0),
        event["kind"].as_str().unwrap_or("?"),
        event["env"].as_str().unwrap_or("-"),
        event["data"]
    )
}

async fn ready(cmds: &mpsc::UnboundedSender<Cmd>, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    while tokio::time::Instant::now() < deadline {
        let (tx, rx) = oneshot::channel();
        if cmds.send(Cmd::Ready { reply: tx }).is_err() {
            return false;
        }
        if rx.await.unwrap_or(false) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(READY_POLL_MS)).await;
    }
    false
}

async fn signals(cmds: mpsc::UnboundedSender<Cmd>) {
    use tokio::signal::unix::{signal, SignalKind};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        return;
    };
    let Ok(mut interrupt) = signal(SignalKind::interrupt()) else {
        return;
    };
    tokio::select! {
        _ = term.recv() => {},
        _ = interrupt.recv() => {},
    }
    let (tx, rx) = oneshot::channel();
    let _ = cmds.send(Cmd::Stop { reply: tx });
    let _ = rx.await;
}

fn daemonize(opts: &StartOpts) -> Result<i32> {
    use std::os::unix::process::CommandExt;

    let home = Home::resolve(opts.home.clone())?;
    home.scaffold()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.log("base"))?;
    let exe = std::env::current_exe().context("locate the tenon binary")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("start")
        .arg("--foreground")
        .arg("--home")
        .arg(&home.root);
    if opts.exit_on_detach {
        command.arg("--exit-on-detach");
    }
    if let Some(dir) = &opts.release_dir {
        command.arg("--release-dir").arg(dir);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log));
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    let mut child = command.spawn().context("start the base process")?;
    let deadline = std::time::Instant::now() + Duration::from_millis(DAEMON_WAIT_MS);
    while std::time::Instant::now() < deadline {
        if home.ready_file().is_file() {
            println!(
                "tenon: base running, pid {}, home {}",
                child.id(),
                home.root.display()
            );
            return Ok(0);
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "base exited with {status}, see {}",
                home.log("base").display()
            );
        }
        std::thread::sleep(Duration::from_millis(READY_POLL_MS));
    }
    let _ = child.kill();
    bail!("base did not come up, see {}", home.log("base").display())
}
