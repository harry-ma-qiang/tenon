use crate::cli_agent::{self, CliAgentSpec, Events, McpEndpoint, RunBudget};
use crate::client::Client;
use crate::config::Config;
use crate::home::Home;
use crate::jail::Limits;
use crate::mcp_loopback::{self, McpServer};
use crate::preflight::{self, Preflight, PreflightSpec};
use crate::ratelimit::RateConfig;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Everything `tenon cli-agent run` takes past the tenon home. `rpm` overrides
/// the account limiter's per-minute cap for this run; `scratch_max_mb` overrides
/// the config default. Wall/step budgets are hard stops.
pub struct RunArgs {
    pub task: String,
    pub model: String,
    pub rpm: Option<u32>,
    pub wall_s: u64,
    pub max_calls: u64,
    pub scratch_max_mb: Option<u64>,
    /// Grant the agent's OWN credential/state dir read-write (RFC P5.0c). Off by
    /// default (the safe floor: creds read-only). Agents like `agy` need it to
    /// refresh their auth token, so a working run turns it on; the hard boundary
    /// (workspace, repo, secrets, `~/.ssh` unreachable) is unchanged either way.
    pub writable_state: bool,
}

/// `tenon cli-agent run`: preflight (mandatory, zero-cost), then run the CLI
/// agent under the host jail with a scratch disk cap, the account rate limiter,
/// the wall/step budget, an optional Tenon-as-MCP loopback server, and SIGINT
/// teardown. base itself runs no agent code — the agent is the jailed child.
pub async fn run(home: Option<PathBuf>, args: RunArgs) -> Result<i32> {
    let home = Home::resolve(home)?;
    let config = Config::load(&home.config_file()).unwrap_or_default();
    let env = config.root_env.clone();
    let cmd = resolve_binary(&args.model)?;
    let agent_home = real_home()?;
    let run_id = format!("{}-{}", args.model, tenon_storage::now());
    let base = home.root.join("cli").join(&run_id);
    let scratch = base.join("scratch");
    let tmp = base.join("tmp");
    std::fs::create_dir_all(&scratch)?;
    std::fs::create_dir_all(&tmp)?;
    let (ro_allow, rw_state) = cred_grants(&args.model, &agent_home, args.writable_state);
    let scratch_max_mb = args
        .scratch_max_mb
        .unwrap_or(config.cli_agent.scratch_max_mb);
    let limits = jail_limits(config.cli_agent.nproc_headroom);

    println!(
        "tenon cli-agent: preflight {} under the jail (zero cost, no paid call)",
        args.model
    );
    let pf = do_preflight(
        &cmd,
        &args.model,
        &scratch,
        &tmp,
        &agent_home,
        &rw_state,
        &ro_allow,
        &limits,
    )
    .await?;
    report_preflight(&args.model, &cmd, &ro_allow, &rw_state, &pf);
    if !pf.ok {
        eprintln!("tenon cli-agent: refusing to spend a paid model call — fix the read-only allowlist and retry preflight");
        return Ok(2);
    }

    let base_up = Client::connect(&home.sock()).await.is_ok();
    let (mcp_endpoint, mcp_server) = start_mcp(&home, &env, base_up).await;
    let agent_args = agent_args(&args.model, &args.task, &scratch);

    let mut rate = RateConfig::default();
    if let Some(rpm) = args.rpm {
        rate.rpm = rpm;
    }
    let spec = CliAgentSpec {
        run: run_id.clone(),
        env: env.clone(),
        cmd: cmd.clone(),
        args: agent_args,
        root: home.root.clone(),
        mcp: mcp_endpoint,
        ro_allow,
        rw_state,
        limits,
        rate,
        budget: RunBudget {
            wall_s: args.wall_s,
            max_steps: args.max_calls,
        },
        extra_env: vec![],
        cgroup_parent: cgroup_parent(),
        agent_home,
        scratch_max_mb,
    };

    let (events, publisher) = event_sink(&home, &env, &run_id, base_up);
    let stop = Arc::new(AtomicBool::new(false));
    let watcher_done = Arc::new(AtomicBool::new(false));
    let stop_watcher =
        spawn_stopfile_watcher(base.join("stop"), stop.clone(), watcher_done.clone());
    write_meta(
        &base,
        json!({
            "run": run_id, "model": args.model, "task": args.task, "env": env,
            "started_ms": tenon_storage::now(), "state": "running",
            "scratch": scratch.display().to_string(), "pid": std::process::id(),
        }),
    )?;

    println!(
        "tenon cli-agent: run {run_id} starting (rpm {}, wall {}s, max-calls {}, scratch cap {scratch_max_mb} MB)",
        spec.rate.rpm, args.wall_s, args.max_calls
    );
    let ev = events.clone();
    let st = stop.clone();
    let mut handle = tokio::task::spawn_blocking(move || cli_agent::run(&spec, ev.as_ref(), &st));
    let joined = tokio::select! {
        res = &mut handle => res,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\ntenon cli-agent: SIGINT — tearing down (jail.kill + mcp stop)");
            stop.store(true, Ordering::Relaxed);
            (&mut handle).await
        }
    };

    watcher_done.store(true, Ordering::Relaxed);
    let _ = stop_watcher.join();
    if let Some(server) = mcp_server {
        server.stop();
    }
    drop(events);
    if let Some(publisher) = publisher {
        let _ = publisher.await;
    }
    let outcome = joined??;
    let state = if outcome.killed { "killed" } else { "done" };
    write_meta(
        &base,
        json!({
            "run": run_id, "model": args.model, "task": args.task, "env": env,
            "state": state, "status": outcome.status, "steps": outcome.steps,
            "killed": outcome.killed, "halted": outcome.halted,
            "scratch": scratch.display().to_string(), "ended_ms": tenon_storage::now(),
        }),
    )?;
    println!(
        "tenon cli-agent: run {run_id} {state} (status {}, steps {}, halted {:?})",
        outcome.status, outcome.steps, outcome.halted
    );
    Ok(if outcome.killed || outcome.status != 0 {
        1
    } else {
        0
    })
}

/// `tenon cli-agent preflight [--model agy]`: the zero-cost auth check on its
/// own. Runs the read-only probes under the jail and reports whether the model
/// is authenticated and which credential paths the jail grants read-only.
pub async fn preflight_cmd(
    home: Option<PathBuf>,
    model: String,
    writable_state: bool,
) -> Result<i32> {
    let home = Home::resolve(home)?;
    let config = Config::load(&home.config_file()).unwrap_or_default();
    let cmd = resolve_binary(&model)?;
    let agent_home = real_home()?;
    let base = home
        .root
        .join("cli")
        .join(format!("preflight-{}", tenon_storage::now()));
    let scratch = base.join("scratch");
    let tmp = base.join("tmp");
    std::fs::create_dir_all(&scratch)?;
    std::fs::create_dir_all(&tmp)?;
    let (ro_allow, rw_state) = cred_grants(&model, &agent_home, writable_state);
    let limits = jail_limits(config.cli_agent.nproc_headroom);
    let pf = do_preflight(
        &cmd,
        &model,
        &scratch,
        &tmp,
        &agent_home,
        &rw_state,
        &ro_allow,
        &limits,
    )
    .await?;
    let _ = std::fs::remove_dir_all(&base);
    report_preflight(&model, &cmd, &ro_allow, &rw_state, &pf);
    Ok(if pf.ok { 0 } else { 2 })
}

/// `tenon cli-agent status`: every run this home has scaffolded, from its
/// `meta.json`, newest state included.
pub async fn status(home: Option<PathBuf>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let mut rows = Vec::new();
    if let Ok(entries) = std::fs::read_dir(home.root.join("cli")) {
        for entry in entries.flatten() {
            if let Ok(body) = std::fs::read_to_string(entry.path().join("meta.json")) {
                if let Ok(value) = serde_json::from_str::<Value>(&body) {
                    rows.push(value);
                }
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({ "runs": rows }))?
    );
    Ok(0)
}

/// `tenon cli-agent stop <run>`: drop the run's stop file, which the running
/// adapter's watcher turns into the kill switch on its next poll.
pub async fn stop(home: Option<PathBuf>, run: String) -> Result<i32> {
    let home = Home::resolve(home)?;
    let base = home.root.join("cli").join(&run);
    if !base.is_dir() {
        bail!("no such cli-agent run {run}");
    }
    std::fs::write(base.join("stop"), "stop")?;
    println!("tenon cli-agent: stop requested for {run}");
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
async fn do_preflight(
    cmd: &str,
    model: &str,
    scratch: &Path,
    tmp: &Path,
    agent_home: &Path,
    rw_allow: &[PathBuf],
    ro_allow: &[PathBuf],
    limits: &Limits,
) -> Result<Preflight> {
    let spec = PreflightSpec {
        cmd: cmd.to_string(),
        probes: probes(model),
        scratch: scratch.to_path_buf(),
        tmp: tmp.to_path_buf(),
        agent_home: agent_home.to_path_buf(),
        rw_allow: rw_allow.to_vec(),
        ro_allow: ro_allow.to_vec(),
        limits: limits.clone(),
        env: probe_env(agent_home, tmp),
    };
    tokio::task::spawn_blocking(move || preflight::run(&spec)).await?
}

fn report_preflight(
    model: &str,
    cmd: &str,
    ro_allow: &[PathBuf],
    rw_state: &[PathBuf],
    pf: &Preflight,
) {
    let show = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if pf.ok {
        println!("tenon cli-agent: preflight CLEAN — {model} authenticated ({cmd})");
        println!("  jail grants read-only: {}", show(ro_allow));
        if !rw_state.is_empty() {
            println!(
                "  jail grants read-write (--writable-state): {}",
                show(rw_state)
            );
        }
        return;
    }
    eprintln!("tenon cli-agent: PREFLIGHT FAILED on `{cmd} {}`", pf.probe);
    if let Some(signature) = &pf.signature {
        eprintln!("  auth-failure signature: {signature:?}");
    }
    let grants = if rw_state.is_empty() {
        format!("read-only ONLY: {}", show(ro_allow))
    } else {
        format!("read-write: {}", show(rw_state))
    };
    eprintln!("  likely a blocked credential path — the jail grants {grants}");
    if rw_state.is_empty() {
        eprintln!("  if the agent needs to refresh its auth token, retry with --writable-state");
    }
    eprintln!("  detail: {}", pf.detail);
}

async fn start_mcp(
    home: &Home,
    env: &str,
    base_up: bool,
) -> (Option<McpEndpoint>, Option<McpServer>) {
    if !base_up {
        println!("tenon cli-agent: base not running — no Tenon-MCP; the agent uses its own tools, confined to scratch (documented fallback)");
        return (None, None);
    }
    match mcp_loopback::start(home, env, crate::token::generate()).await {
        Ok(server) => {
            println!("tenon cli-agent: Tenon-as-MCP on {}", server.url);
            let endpoint = McpEndpoint {
                url: server.url.clone(),
                token: server.token.clone(),
            };
            (Some(endpoint), Some(server))
        }
        Err(error) => {
            eprintln!("tenon cli-agent: MCP loopback did not start ({error}); falling back to the agent's jailed tools");
            (None, None)
        }
    }
}

type Publisher = Option<tokio::task::JoinHandle<()>>;

fn event_sink(home: &Home, env: &str, run: &str, base_up: bool) -> (Arc<ClientEvents>, Publisher) {
    if !base_up {
        return (
            Arc::new(ClientEvents {
                run: run.to_string(),
                tx: None,
            }),
            None,
        );
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
    let sock = home.sock();
    let env = env.to_string();
    let run_owned = run.to_string();
    let publisher = tokio::spawn(async move {
        let Ok(mut client) = Client::connect(&sock).await else {
            return;
        };
        while let Some((kind, data)) = rx.recv().await {
            let envelope = json!({
                "topic": format!("cli-agent/{run_owned}/{kind}"),
                "env": env, "src": "cli-agent", "durable": false, "payload": data,
            });
            let _ = client
                .call("bus.publish", json!({ "envelope": envelope }))
                .await;
        }
    });
    (
        Arc::new(ClientEvents {
            run: run.to_string(),
            tx: Some(tx),
        }),
        Some(publisher),
    )
}

/// The run's event sink: print every event as a line (so the human watching the
/// run sees the trace) and, when base is up, forward it to the shared bus under
/// `cli-agent/<run>/<kind>` through the front door.
struct ClientEvents {
    run: String,
    tx: Option<tokio::sync::mpsc::UnboundedSender<(String, Value)>>,
}

impl Events for ClientEvents {
    fn emit(&self, kind: &str, data: Value) {
        println!("[cli-agent/{}/{}] {}", self.run, kind, data);
        if let Some(tx) = &self.tx {
            let _ = tx.send((kind.to_string(), data));
        }
    }
}

fn spawn_stopfile_watcher(
    path: PathBuf,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            if path.exists() {
                stop.store(true, Ordering::Relaxed);
                return;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    })
}

fn agent_args(model: &str, task: &str, scratch: &Path) -> Vec<String> {
    match model {
        "claude" => vec![
            "-p".to_string(),
            task.to_string(),
            "--dangerously-skip-permissions".to_string(),
        ],
        _ => vec![
            "-p".to_string(),
            task.to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--log-file".to_string(),
            scratch.join("agy.log").display().to_string(),
        ],
    }
}

/// The zero-cost preflight probes. `--version` and `mcp list` prove the binary
/// starts; `models` is the auth probe — it hits the credential token source
/// (`fetchAvailableModels`) and prints "not logged in" when the jail blocks a
/// cred path, without ever spending a model completion. `models` is what catches
/// a broken auth that the first two miss.
fn probes(model: &str) -> Vec<Vec<String>> {
    match model {
        "claude" => vec![vec!["--version".to_string()]],
        _ => vec![
            vec!["--version".to_string()],
            vec!["mcp".to_string(), "list".to_string()],
            vec!["models".to_string()],
        ],
    }
}

fn probe_env(agent_home: &Path, tmp: &Path) -> Vec<(String, String)> {
    let home = agent_home.display().to_string();
    vec![
        ("HOME".to_string(), home.clone()),
        ("TMPDIR".to_string(), tmp.display().to_string()),
        (
            "PATH".to_string(),
            format!(
                "{home}/.local/bin:{home}/.local/share/mise/shims:/usr/local/bin:/usr/bin:/bin"
            ),
        ),
    ]
}

/// The agent's own credential/state dirs, split into the read-only and
/// read-write grants for the jail. With `writable_state` off (the safe default)
/// every cred dir is read-only; with it on they are read-write, which some
/// agents (agy) need to refresh their auth token. Never returns `~/workspace`,
/// the repo, secrets, or `~/.ssh` — those stay unreachable in either mode.
fn cred_grants(model: &str, home: &Path, writable_state: bool) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let ro = match model {
        "claude" => vec![home.join(".claude"), home.join(".config").join("claude")],
        _ => vec![
            home.join(".gemini"),
            home.join(".cache").join("antigravity"),
        ],
    };
    if !writable_state {
        let mut ro = ro;
        ro.retain(|path| path.exists());
        return (ro, Vec::new());
    }
    // The writable set is the agent's own state dir plus its cache: agy refreshes
    // its auth token in `~/.gemini` and spawns a browser/language-server sidecar
    // out of `~/.cache`. None of these is the user's workspace, repo, or `~/.ssh`.
    let mut rw = match model {
        "claude" => vec![home.join(".claude"), home.join(".config").join("claude")],
        _ => vec![home.join(".gemini"), home.join(".cache")],
    };
    rw.retain(|path| path.exists());
    (Vec::new(), rw)
}

/// The jail rlimits for a cli-agent run. `RLIMIT_AS` is deliberately left OFF
/// (`mem_bytes = 0`): agy and claude are Go/Node binaries whose runtimes reserve
/// huge *virtual* address space, so a tight `RLIMIT_AS` triggers a false
/// `fatal error: out of memory` before the agent ever runs. Real memory capping
/// is the cgroup's `memory.max` (`mem_max`), enforced when base runs under the
/// delegated user manager; here it degrades to no memory cap (documented), while
/// `RLIMIT_NPROC` (fork bombs), `RLIMIT_CPU`, `RLIMIT_NOFILE`, the scratch disk
/// watcher and the wall/step budget remain the floor.
fn jail_limits(nproc_headroom: u64) -> Limits {
    Limits {
        nproc: uid_process_count() + nproc_headroom,
        mem_bytes: 0,
        ..Limits::default()
    }
}

/// The current per-uid process count. `RLIMIT_NPROC` is per-uid, not per-tree,
/// so the jail's ceiling is this plus a headroom — never an absolute that would
/// either starve the agent or fail to cap a fork bomb relative to real load.
fn uid_process_count() -> u64 {
    let uid = unsafe { libc::geteuid() };
    let mut count = 0u64;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return count;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(entry.path()) {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() == uid {
                count += 1;
            }
        }
    }
    count
}

fn cgroup_parent() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let parent = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"
    ));
    parent.is_dir().then_some(parent)
}

fn resolve_binary(model: &str) -> Result<String> {
    let name = match model {
        "agy" => "agy",
        "claude" => "claude",
        other => bail!("unknown model {other}, expected agy or claude"),
    };
    which(name).ok_or_else(|| anyhow::anyhow!("{name} not found on PATH"))
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.display().to_string())
}

fn real_home() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?,
    ))
}

fn write_meta(base: &Path, meta: Value) -> Result<()> {
    std::fs::create_dir_all(base)?;
    std::fs::write(base.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}
