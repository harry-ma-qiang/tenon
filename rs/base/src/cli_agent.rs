use crate::jail::{self, JailSpec, Limits};
use crate::ratelimit::{Clock, Grant, Limiter, RateConfig, SystemClock};
use git2::{IndexAddOption, Repository, RepositoryInitOptions, Signature};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

const CONTRACT_NAME: &str = "cli-agent";
const POLL: Duration = Duration::from_millis(200);
const ACQUIRE_SLICE_MS: u64 = 100;

/// The Tenon-as-MCP-server endpoint the jailed agent is pointed at, so its only
/// tools are Tenon's sandboxed ones (RFC layer B). HTTP over loopback because the
/// jail's only reachable Tenon surface is the serve port — the front-door unix
/// socket lives under `~/.tenon/run`, which the jail deliberately does not grant.
#[derive(Debug, Clone)]
pub struct McpEndpoint {
    pub url: String,
    pub token: String,
}

/// A budget for one cli-agent run, on top of the account rate limiter: wall time
/// and a step ceiling, both hard stops that SIGKILL the jail. `0` disables one.
#[derive(Debug, Clone, Default)]
pub struct RunBudget {
    pub wall_s: u64,
    pub max_steps: u64,
}

/// `{kind: "cli-agent", cmd, args, scratch, mcp, rate, budget}` made concrete.
/// `root` is the tenon home; the scratch and per-run tmp are derived beneath it.
pub struct CliAgentSpec {
    pub run: String,
    pub env: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub root: PathBuf,
    pub mcp: Option<McpEndpoint>,
    pub ro_allow: Vec<PathBuf>,
    /// Read-write credential/state dirs (the `--writable-state` opt-in). Empty by
    /// default; when set, the agent's own state dir is writable so it can refresh
    /// its auth token. Never the user's workspace, repo, secrets or `~/.ssh`.
    pub rw_state: Vec<PathBuf>,
    pub limits: Limits,
    pub rate: RateConfig,
    pub budget: RunBudget,
    pub extra_env: Vec<(String, String)>,
    pub cgroup_parent: Option<PathBuf>,
    /// The `HOME` the agent sees. For `agy`/`claude` this is the real user home
    /// so the agent finds its own credential dir (`ro_allow` grants it read-only);
    /// tests point it at scratch. Never `~/workspace` — that stays unreachable.
    pub agent_home: PathBuf,
    /// The scratch disk ceiling in MB, enforced by a background watcher that
    /// SIGKILLs the jail if the scratch tree grows past it (so an overnight agent
    /// cannot fill the host disk). `0` disables the watcher.
    pub scratch_max_mb: u64,
}

impl CliAgentSpec {
    pub fn scratch(&self) -> PathBuf {
        self.root.join("cli").join(&self.run).join("scratch")
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("cli").join(&self.run).join("tmp")
    }

    /// The runtime-contract manifest this run registers under (RFC section 2), so
    /// `tenon status`/`tree` can show a cli-agent runtime beside the default one.
    pub fn manifest(&self) -> Value {
        json!({
            "manifest": {"name": CONTRACT_NAME, "version": "0.1.0", "hash": self.run},
            "health": {"kind": "http", "target": "cli-agent"},
            "channels": {
                "events": format!("cli-agent/{}/step", self.run),
                "approvals": "approval.request",
            },
        })
    }
}

/// Where the adapter sends its bus events. Every event lands on
/// `cli-agent/<run>/<kind>` (started, step, tool-call, output, done, error), so a
/// subscriber can range one run's trace exactly as it ranges a session's log.
pub trait Events: Send + Sync {
    fn emit(&self, kind: &str, data: Value);
}

/// The production sink: publish onto the shared hub under the run's topic.
pub struct HubEvents {
    pub hub: Arc<tenon_bus::Hub>,
    pub env: String,
    pub run: String,
}

impl Events for HubEvents {
    fn emit(&self, kind: &str, data: Value) {
        let topic = format!("cli-agent/{}/{}", self.run, kind);
        let mut envelope = tenon_bus::Envelope::new(topic, tenon_bus::Level::Info, data);
        envelope.env = Some(self.env.clone());
        envelope.src = "cli-agent".to_string();
        self.hub.emit(envelope);
    }
}

#[derive(Debug, Default)]
pub struct RunOutcome {
    pub status: i32,
    pub steps: u64,
    pub killed: bool,
    pub halted: Option<String>,
}

/// Run one cli-agent under the host jail (RFC P5.0a/b). Blocking, so a caller
/// puts it on a blocking task. `stop` is the kill switch: set it and the jail is
/// SIGKILLed and the run ends with `killed`. Never runs agent code in this
/// process — the agent is the jailed child and this only scaffolds, streams and
/// enforces. The real `agy`/`claude` invocation is a human-triggered step; tests
/// stand a fake script in for the agent.
pub fn run(
    spec: &CliAgentSpec,
    events: &dyn Events,
    stop: &AtomicBool,
) -> anyhow::Result<RunOutcome> {
    let limiter = Mutex::new(Limiter::new(spec.rate.clone()));
    let clock = SystemClock;
    run_with(spec, events, stop, &limiter, &clock)
}

pub fn run_with(
    spec: &CliAgentSpec,
    events: &dyn Events,
    stop: &AtomicBool,
    limiter: &Mutex<Limiter>,
    clock: &dyn Clock,
) -> anyhow::Result<RunOutcome> {
    let scratch = spec.scratch();
    let tmp = spec.tmp();
    std::fs::create_dir_all(&scratch)?;
    std::fs::create_dir_all(&tmp)?;
    write_mcp_config(&scratch, spec)?;
    snap_init(&scratch)?;

    if let Some(reason) = wait_for_slot(limiter, clock, stop) {
        events.emit("error", json!({"run": spec.run, "reason": reason.clone()}));
        return Ok(RunOutcome {
            halted: Some(reason),
            ..Default::default()
        });
    }

    events.emit(
        "started",
        json!({
            "run": spec.run,
            "cmd": spec.cmd,
            "scratch": scratch.display().to_string(),
            "env": spec.env,
        }),
    );

    let jail_spec = JailSpec {
        cmd: spec.cmd.clone(),
        args: spec.args.clone(),
        cwd: scratch.clone(),
        scratch: scratch.clone(),
        tmp: tmp.clone(),
        rw_allow: spec.rw_state.clone(),
        ro_allow: spec.ro_allow.clone(),
        env: child_env(spec, &tmp),
        limits: spec.limits.clone(),
        cgroup_parent: spec.cgroup_parent.clone(),
    };
    let mut jail = match jail::spawn(&jail_spec) {
        Ok(jail) => jail,
        Err(error) => {
            let reason = error.to_string();
            events.emit("error", json!({"run": spec.run, "reason": reason.clone()}));
            limiter.lock().expect("limiter").release();
            return Ok(RunOutcome {
                status: -1,
                halted: Some(reason),
                ..Default::default()
            });
        }
    };

    let outcome = pump(spec, events, stop, limiter, clock, &scratch, &mut jail);
    limiter.lock().expect("limiter").release();
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn pump(
    spec: &CliAgentSpec,
    events: &dyn Events,
    stop: &AtomicBool,
    limiter: &Mutex<Limiter>,
    clock: &dyn Clock,
    scratch: &Path,
    jail: &mut jail::Jail,
) -> RunOutcome {
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = jail.child.stdout.take();
    let stderr = jail.child.stderr.take();
    let reader = stdout.map(|out| {
        let tx = tx.clone();
        std::thread::spawn(move || pipe_lines(out, tx))
    });
    // Drop the original sender so the channel disconnects the moment the reader
    // finishes (the agent closed stdout); otherwise `recv_timeout` never ends.
    drop(tx);
    let draining = stderr.map(|err| std::thread::spawn(move || drain(err)));

    let over_quota = Arc::new(AtomicBool::new(false));
    let watcher_done = Arc::new(AtomicBool::new(false));
    let watcher = spawn_disk_watcher(
        scratch,
        spec.scratch_max_mb,
        over_quota.clone(),
        watcher_done.clone(),
    );

    let started = Instant::now();
    let mut steps = 0u64;
    let mut killed = false;
    let mut halted: Option<String> = None;
    loop {
        if over_quota.load(Ordering::Relaxed) {
            let reason = format!("scratch disk cap exceeded ({} MB)", spec.scratch_max_mb);
            events.emit(
                "violation",
                json!({"run": spec.run, "reason": reason.clone()}),
            );
            halted = Some(reason);
            jail.kill();
            killed = true;
            break;
        }
        match rx.recv_timeout(POLL) {
            Ok(line) => {
                handle_line(
                    spec,
                    events,
                    limiter,
                    clock,
                    scratch,
                    &line,
                    &mut steps,
                    &mut halted,
                );
                if halted.is_some() || (spec.budget.max_steps > 0 && steps >= spec.budget.max_steps)
                {
                    let reason = halted
                        .clone()
                        .unwrap_or_else(|| "step budget reached".to_string());
                    halted.get_or_insert(reason);
                    jail.kill();
                    killed = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    jail.kill();
                    killed = true;
                    break;
                }
                if spec.budget.wall_s > 0 && started.elapsed().as_secs() >= spec.budget.wall_s {
                    halted = Some("wall budget reached".to_string());
                    jail.kill();
                    killed = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    watcher_done.store(true, Ordering::Relaxed);
    if let Some(watcher) = watcher {
        let _ = watcher.join();
    }
    if let Some(reader) = reader {
        let _ = reader.join();
    }
    if let Some(draining) = draining {
        let _ = draining.join();
    }
    let status = jail.child.wait().map(status_code).unwrap_or(-1);
    let snap = snap_commit(scratch, &format!("final step {steps}")).unwrap_or(Value::Null);
    if killed {
        limiter.lock().expect("limiter").record_success();
        events.emit(
            "error",
            json!({"run": spec.run, "reason": halted.clone().unwrap_or_else(|| "killed".to_string()), "killed": true, "snapshot": snap}),
        );
    } else {
        limiter.lock().expect("limiter").record_success();
        events.emit(
            "done",
            json!({"run": spec.run, "status": status, "steps": steps, "snapshot": snap}),
        );
    }
    RunOutcome {
        status,
        steps,
        killed,
        halted,
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_line(
    spec: &CliAgentSpec,
    events: &dyn Events,
    limiter: &Mutex<Limiter>,
    clock: &dyn Clock,
    scratch: &Path,
    line: &str,
    steps: &mut u64,
    halted: &mut Option<String>,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let parsed = serde_json::from_str::<Value>(line).ok();
    let kind = parsed
        .as_ref()
        .and_then(|value| value.get("type").or_else(|| value.get("event")))
        .and_then(Value::as_str)
        .unwrap_or("output");
    match kind {
        "tool_use" | "tool-call" | "tool_call" => {
            *steps += 1;
            let name = parsed
                .as_ref()
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            let snap = snap_commit(scratch, &format!("step {}", *steps)).unwrap_or(Value::Null);
            events.emit(
                "tool-call",
                json!({"run": spec.run, "step": *steps, "name": name, "snapshot": snap}),
            );
        }
        "step" | "turn" => {
            *steps += 1;
            let snap = snap_commit(scratch, &format!("step {}", *steps)).unwrap_or(Value::Null);
            events.emit(
                "step",
                json!({"run": spec.run, "step": *steps, "snapshot": snap}),
            );
        }
        "error" => {
            let rate_limited = parsed
                .as_ref()
                .and_then(|value| value.get("rate_limited"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if rate_limited {
                let outcome = limiter
                    .lock()
                    .expect("limiter")
                    .record(clock.now_ms(), true);
                if outcome.opened {
                    events.emit(
                        "violation",
                        json!({"run": spec.run, "reason": "rate limit hit, breaker opened"}),
                    );
                }
                if outcome.halted {
                    *halted = limiter
                        .lock()
                        .expect("limiter")
                        .halted()
                        .map(str::to_string);
                }
            }
            events.emit(
                "output",
                json!({"run": spec.run, "step": *steps, "text": line}),
            );
        }
        _ => {
            let text = parsed
                .as_ref()
                .and_then(|value| value.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| line.to_string());
            events.emit(
                "output",
                json!({"run": spec.run, "step": *steps, "text": text}),
            );
        }
    }
}

/// Block until the account limiter grants a slot, `stop` is set, or the breaker
/// halts. Returns `Some(reason)` when the run must not start at all.
fn wait_for_slot(limiter: &Mutex<Limiter>, clock: &dyn Clock, stop: &AtomicBool) -> Option<String> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Some("kill switch set before start".to_string());
        }
        let grant = limiter.lock().expect("limiter").try_acquire(clock);
        match grant {
            Grant::Allow => return None,
            Grant::Halt(reason) => return Some(reason),
            Grant::Wait(_) => std::thread::sleep(Duration::from_millis(ACQUIRE_SLICE_MS)),
        }
    }
}

fn child_env(spec: &CliAgentSpec, tmp: &Path) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_string(), spec.agent_home.display().to_string()),
        ("TMPDIR".to_string(), tmp.display().to_string()),
        (
            "PATH".to_string(),
            format!(
                "{}/.local/bin:{}/.local/share/mise/shims:/usr/local/bin:/usr/bin:/bin",
                home_dir(),
                home_dir()
            ),
        ),
        ("TENON_ENV".to_string(), spec.env.clone()),
        ("TENON_CLI_RUN".to_string(), spec.run.clone()),
    ];
    if let Some(mcp) = &spec.mcp {
        env.push(("TENON_MCP_URL".to_string(), mcp.url.clone()));
        env.push(("TENON_MCP_TOKEN".to_string(), mcp.token.clone()));
    }
    env.extend(spec.extra_env.iter().cloned());
    env
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())
}

/// Write the standard MCP client config into the scratch cwd. Both `agy` and
/// `claude` read an `mcpServers` map; a project `.mcp.json` in the working
/// directory is the portable form. The exact `agy mcp add` command to register
/// the same server by hand is written beside it, for the human-triggered run.
fn write_mcp_config(scratch: &Path, spec: &CliAgentSpec) -> anyhow::Result<()> {
    let Some(mcp) = &spec.mcp else {
        return Ok(());
    };
    let config = json!({
        "mcpServers": {
            "tenon": {
                "type": "http",
                "url": mcp.url,
                "headers": {"Authorization": format!("Bearer {}", mcp.token)},
            }
        }
    });
    std::fs::write(
        scratch.join(".mcp.json"),
        serde_json::to_string_pretty(&config)?,
    )?;
    let register = format!(
        "#!/bin/sh\n# Register Tenon-as-MCP with agy (or claude) for this run.\n\
         agy mcp add --header \"Authorization: Bearer {}\" tenon {}\n",
        mcp.token, mcp.url
    );
    std::fs::write(scratch.join("mcp-register.sh"), register)?;
    Ok(())
}

/// A best-effort scratch disk guard: poll the scratch tree's size and, the first
/// time it crosses the cap, raise `over_quota` so the pump SIGKILLs the jail. It
/// is a floor for the unprivileged case where a size-limited tmpfs is not
/// mountable — the agent cannot fill the host disk overnight either way.
fn spawn_disk_watcher(
    scratch: &Path,
    max_mb: u64,
    over_quota: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    if max_mb == 0 {
        return None;
    }
    let scratch = scratch.to_path_buf();
    let max_bytes = max_mb.saturating_mul(1024 * 1024);
    Some(std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            if dir_size(&scratch) > max_bytes {
                over_quota.store(true, Ordering::Relaxed);
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }))
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total += dir_size(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

fn pipe_lines(reader: impl std::io::Read, tx: mpsc::Sender<String>) {
    use std::io::BufRead;
    let buffered = std::io::BufReader::new(reader);
    for line in buffered.lines().map_while(std::result::Result::ok) {
        if tx.send(line).is_err() {
            break;
        }
    }
}

fn drain(mut reader: impl std::io::Read) {
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink);
}

fn status_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// The snapshot repository lives beside the scratch tree, not inside it, so the
/// worktree the agent writes to never contains the GIT_DIR — the one shape that
/// makes `add_all` reject its own git directory.
fn snap_dir(scratch: &Path) -> PathBuf {
    scratch.parent().unwrap_or(scratch).join("snap.git")
}

fn snap_init(scratch: &Path) -> anyhow::Result<()> {
    let git_dir = snap_dir(scratch);
    if git_dir.exists() {
        return Ok(());
    }
    let mut opts = RepositoryInitOptions::new();
    opts.no_reinit(true);
    opts.bare(false);
    opts.workdir_path(scratch);
    let repo = Repository::init_opts(&git_dir, &opts)?;
    let info = git_dir.join("info");
    std::fs::create_dir_all(&info)?;
    std::fs::write(info.join("exclude"), ".mcp.json\n")?;
    drop(repo);
    Ok(())
}

/// A parentless snapshot commit of the scratch tree — the same reversible,
/// self-contained shape the worker uses, so every step the agent takes is
/// restorable. Best effort: a snapshot failure never fails the run.
fn snap_commit(scratch: &Path, label: &str) -> anyhow::Result<Value> {
    let git_dir = snap_dir(scratch);
    let repo = Repository::open(&git_dir)?;
    let mut index = repo.index()?;
    index.add_all(["*"].iter(), IndexAddOption::DEFAULT, None)?;
    index.update_all(["*"].iter(), None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let who = Signature::now("tenon", "cli-agent@tenon.local")?;
    let oid = repo.commit(None, &who, &who, label, &tree, &[])?;
    repo.reference("refs/heads/snap", oid, true, label)?;
    Ok(json!({"ref": oid.to_string(), "label": label}))
}
