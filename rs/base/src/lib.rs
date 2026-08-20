pub mod approvals;
#[cfg(feature = "http")]
pub mod auth;
pub mod base;
pub mod bench;
pub mod blob;
pub mod bluegreen;
pub mod budget;
pub mod bus;
pub mod candidate;
pub mod check;
pub mod client;
pub mod cmds;
pub mod config;
pub mod drive;
pub mod envfiber;
pub mod envrpc;
pub mod facaderpc;
pub mod frame;
pub mod harness;
pub mod hash;
pub mod home;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "http")]
pub mod ingress;
pub mod instance;
pub mod integrity;
pub mod kv;
pub mod lock;
pub mod manifest;
pub mod node;
pub mod params;
pub mod peer;
pub mod privilege;
pub mod probes;
#[cfg(feature = "http")]
pub mod proxy;
pub mod query;
pub mod release;
pub mod rpc;
pub mod run;
pub mod runtime;
#[cfg(feature = "http")]
pub mod secret;
pub mod server;
pub mod service;
pub mod signals;
pub mod snap;
pub mod spawn;
pub mod state;
pub mod timer;
#[cfg(feature = "http")]
pub mod tls;
pub mod token;
pub mod tui;
pub mod ui;
pub mod upgrade;
pub mod worker;
#[cfg(feature = "http")]
pub mod ws;

use crate::client::Client;
use crate::config::Config;
use crate::home::Home;
use crate::lock::Lock;
use crate::rpc::Cmd;
use crate::signals::Signals;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::Sandbox;
use tenon_storage::Store;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

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
    let _lock = match Lock::try_acquire(&home)? {
        Some(lock) => lock,
        None => {
            let pid = Lock::holder_pid(&home).unwrap_or(0);
            bail!("already running (pid {pid})");
        }
    };
    let sock = home.sock();
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(home.ready_file());
    let mut signals = Signals::install()?;

    let config = Config::load(&home.config_file())?;
    home.prepare(&config.root_env)?;
    let release = release::resolve(
        &home,
        opts.release_dir.as_deref(),
        opts.payload,
        opts.version,
    )?;
    if integrity::restore_if_corrupt(&home.state_file(), &home.lkg_state_file())? {
        eprintln!("tenon: state.sqlite was corrupt, restored from lkg");
    }
    let store = Store::open(&home.state_file())?;
    let sandbox: Arc<dyn Sandbox> = Arc::from(tenon_sandbox::backend(&config.sandbox)?);
    let listener = UnixListener::bind(&sock).with_context(|| format!("bind {}", sock.display()))?;

    let (cmds, cmd_rx) = mpsc::unbounded_channel();
    let (exits, exit_rx) = mpsc::unbounded_channel();
    spawn_reap(sandbox.clone(), home.hash(), cmds.clone());
    budget::watch_stop_file(home.run().join(budget::STOP_FILE), cmds.clone());
    Signals::kill_switch(cmds.clone())?;
    budget::ticker(
        Duration::from_millis(config.budget_tick_ms.max(500)),
        cmds.clone(),
    );
    let facades = bus::Facades::build(&home)?;
    let mut state = base::Base::new(
        home.clone(),
        config.clone(),
        store,
        release.clone(),
        sandbox,
        opts.exit_on_detach,
        exits,
        cmds.clone(),
    );
    state.hub = Some(facades.hub.clone());
    #[cfg(feature = "http")]
    {
        state.kv = Some(facades.kv.clone());
        ingress::spawn_liveness(facades.kv.clone(), config.ingress.probe_ms);
    }
    tokio::spawn(server::serve(
        listener,
        cmds.clone(),
        server::Opts {
            root_env: config.root_env.clone(),
            timeout: Duration::from_millis(config.request_timeout_ms),
            facades: Some(facades),
        },
    ));
    let actor = tokio::spawn(state.run(cmd_rx, exit_rx));

    tokio::select! {
        _ = signals.recv() => {
            return shutdown_during_boot(cmds, actor).await;
        }
        outcome = boot_until_ready(&cmds, Duration::from_millis(config.boot_timeout_ms)) => {
            if let Err(error) = outcome {
                let (tx, rx) = oneshot::channel();
                let _ = cmds.send(Cmd::Stop { reply: tx });
                let _ = rx.await;
                let _ = actor.await;
                return Err(error);
            }
        }
    }

    write_ready(&home, std::process::id())?;
    println!(
        "tenon: base ready, pid {}, home {}, release {}",
        std::process::id(),
        home.root.display(),
        release.display()
    );
    tokio::spawn(async move {
        signals.recv().await;
        let (tx, rx) = oneshot::channel();
        let _ = cmds.send(Cmd::Stop { reply: tx });
        let _ = rx.await;
    });
    Ok(actor.await.unwrap_or(1))
}

/// Sweeps stale sandbox containers for this home on a blocking-pool thread, never
/// the actor's own task, so a slow `podman ps`/`inspect`/`rm -f` round trip can
/// never delay `Cmd::Boot` or a boot-time signal. Fire-and-forget: the result
/// reaches the actor as a `Cmd` once it is ready, whenever that is.
fn spawn_reap(sandbox: Arc<dyn Sandbox>, home_hash: String, cmds: mpsc::UnboundedSender<Cmd>) {
    tokio::task::spawn_blocking(move || {
        let count = sandbox.reap(&home_hash, false).unwrap_or(0);
        let _ = cmds.send(Cmd::SandboxReaped { count });
    });
}

async fn boot_until_ready(cmds: &mpsc::UnboundedSender<Cmd>, timeout: Duration) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    cmds.send(Cmd::Boot { reply: tx })
        .map_err(|_| anyhow::anyhow!("base actor gone"))?;
    rx.await?.map_err(|error| anyhow::anyhow!(error))?;
    if !ready(cmds, timeout).await {
        bail!(
            "the nodes did not register within {} ms",
            timeout.as_millis()
        );
    }
    Ok(())
}

async fn shutdown_during_boot(
    cmds: mpsc::UnboundedSender<Cmd>,
    actor: JoinHandle<i32>,
) -> Result<i32> {
    let (tx, rx) = oneshot::channel();
    let _ = cmds.send(Cmd::AbortBoot { reply: tx });
    let _ = rx.await;
    let _ = actor.await;
    bail!("interrupted during boot")
}

fn write_ready(home: &Home, pid: u32) -> Result<()> {
    let tmp = home.ready_tmp_file();
    std::fs::write(&tmp, pid.to_string())?;
    std::fs::rename(&tmp, home.ready_file())?;
    Ok(())
}

pub async fn attach(home: Option<PathBuf>, env: Option<String>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let mut client = Client::connect(&home.sock()).await?;
    let status = client.call("status", json!({})).await?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    let _ = &env;
    let subscribed = client
        .call(
            "bus.subscribe",
            json!({"topics": ["session/**", "base/**"], "coalesce_ms": 16}),
        )
        .await?;
    println!("tenon: attached from offset {}", subscribed["offset"]);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(0),
            event = client.next_ev() => match event? {
                None => return Ok(0),
                Some(event) => println!("{}", line(&event)),
            },
        }
    }
}

/// `tenon approvals`: the queue as a human reads it, newest last. The same
/// rows `approval.list` answers with, one line each.
pub async fn approvals(home: Option<PathBuf>, status: Option<String>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let mut client = Client::connect(&home.sock()).await?;
    let status = status.unwrap_or_else(|| "pending".to_string());
    let result = client
        .call("approval.list", json!({ "status": status }))
        .await?;
    let rows = result["approvals"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("tenon: no {status} approvals");
        return Ok(0);
    }
    for row in rows {
        println!(
            "{} {} {} {} {}",
            row["id"],
            row["status"].as_str().unwrap_or("?"),
            row["env"].as_str().unwrap_or("-"),
            row["kind"].as_str().unwrap_or("-"),
            row["reason"].as_str().unwrap_or_default()
        );
    }
    Ok(0)
}

/// `tenon approve <id>`: the human half of the gate. `--deny` is the other
/// verdict; both release whatever call is blocked on the row.
pub async fn approve(
    home: Option<PathBuf>,
    id: i64,
    deny: bool,
    note: Option<String>,
) -> Result<i32> {
    let mut params = json!({
        "approval_id": id,
        "decision": match deny { true => "deny", false => "approve" },
    });
    if let Some(note) = note {
        params["note"] = json!(note);
    }
    rpc(home, "approval.answer", params).await
}

/// `tenon rollback`: the LKG manifest is verified before anything is put
/// back, and a mismatch names what differs instead of restoring over it.
pub async fn rollback(home: Option<PathBuf>, force: bool) -> Result<i32> {
    let home = Home::resolve(home)?;
    let result = manifest::rollback(&home, force)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(0)
}

/// `tenon status --lkg`: what the last promotion pinned and whether the
/// copies on disk still hash to it. Needs no running base.
pub fn lkg_status(home: Option<PathBuf>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let manifest = manifest::read(&home)?;
    let differs = manifest::verify(&home, &manifest);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "lkg": home.lkg(),
            "manifest": manifest,
            "verified": differs.is_empty(),
            "differs": differs,
        }))?
    );
    Ok(match differs.is_empty() {
        true => 0,
        false => 1,
    })
}

pub async fn rpc(home: Option<PathBuf>, method: &str, params: Value) -> Result<i32> {
    let home = Home::resolve(home)?;
    let mut client = Client::connect(&home.sock()).await?;
    let result = client.call(method, params).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(0)
}

/// The human-facing reap: works whether or not base is running, since it opens
/// the sandbox backend directly rather than going through the actor. `all`
/// removes every container for this home regardless of whether its `tenon.base`
/// pid is alive; without it only containers whose base is confirmed dead go.
pub async fn sandbox_reap(home: Option<PathBuf>, all: bool) -> Result<i32> {
    let home = Home::resolve(home)?;
    let config = if home.config_file().is_file() {
        Config::load(&home.config_file())?
    } else {
        Config::default()
    };
    let sandbox = tenon_sandbox::backend(&config.sandbox)?;
    let count = tokio::task::spawn_blocking(move || sandbox.reap(&home.hash(), all))
        .await
        .map_err(|error| anyhow::anyhow!(error))??;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"reaped": count}))?
    );
    Ok(0)
}

/// Unpacks an OCI image into this home's image store. A local operation like
/// `rollback`: it needs no base, only the home, because a root filesystem is a
/// human's input to a boot rather than something a running system fetches.
pub async fn image_pull(home: Option<PathBuf>, reference: &str, name: Option<&str>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let images = home.images_dir();
    std::fs::create_dir_all(&images)?;
    let name = name.unwrap_or("default").to_string();
    let reference = reference.to_string();
    let rootfs = tokio::task::spawn_blocking(move || {
        tenon_sandbox::krun::image::pull(&images, &reference, &name)
    })
    .await
    .map_err(|error| anyhow::anyhow!(error))??;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"rootfs": rootfs.display().to_string()}))?
    );
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

fn daemonize(opts: &StartOpts) -> Result<i32> {
    use std::os::unix::process::CommandExt;

    let home = Home::resolve(opts.home.clone())?;
    home.scaffold()?;
    match Lock::try_acquire(&home)? {
        Some(lock) => {
            let _ = std::fs::remove_file(home.sock());
            let _ = std::fs::remove_file(home.ready_file());
            drop(lock);
        }
        None => {
            let pid = Lock::holder_pid(&home).unwrap_or(0);
            bail!("already running (pid {pid})");
        }
    }
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
