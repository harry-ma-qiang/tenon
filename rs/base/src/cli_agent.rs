use crate::ratelimit::{Clock, Grant, Limiter, SystemClock};
use git2::{IndexAddOption, Repository, RepositoryInitOptions, Signature};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tenon_sandbox::{ExecSpec, Instance};

const CONTRACT_NAME: &str = "cli-agent";
const POLL: Duration = Duration::from_millis(200);
const ACQUIRE_SLICE_MS: u64 = 100;

/// A budget for one cli-agent run, on top of the account rate limiter: wall time
/// and a step ceiling, both hard stops that tear down the container. `0` disables
/// one.
#[derive(Debug, Clone, Default)]
pub struct RunBudget {
    pub wall_s: u64,
    pub max_steps: u64,
}

/// One sandbox-native cli-agent run made concrete (RFC P5.0-v2). The agent runs
/// INSIDE an already-spawned OCI instance; `cmd`/`args` are the argv exec'd in
/// the container, `guest_cwd` the working directory there (the mounted
/// workspace), `agent_env` what the agent needs (HOME, PATH, machine wiring).
/// `workspace` is the HOST side of that mount, where snapshots are taken and the
/// worker/host sees the agent's edits.
pub struct CliAgentSpec {
    pub run: String,
    pub env: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub workspace: PathBuf,
    pub guest_cwd: String,
    pub agent_env: Vec<(String, String)>,
    pub rate: crate::ratelimit::RateConfig,
    pub budget: RunBudget,
}

impl CliAgentSpec {
    fn exec_spec(&self) -> ExecSpec {
        ExecSpec {
            cmd: self.cmd.clone(),
            args: self.args.clone(),
            env: self.agent_env.clone(),
            cwd: Some(self.guest_cwd.clone()),
        }
    }

    /// The runtime-contract manifest this run registers under (RFC section 2), so
    /// `tenon status`/`tree` can show a cli-agent runtime beside the default one.
    pub fn manifest(&self) -> Value {
        json!({
            "manifest": {"name": CONTRACT_NAME, "version": "0.2.0", "hash": self.run},
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

/// Run one cli-agent INSIDE `instance` (RFC P5.0-v2). Blocking, so a caller puts
/// it on a blocking task. `stop` is the kill switch: set it and the container is
/// destroyed and the run ends `killed`. base runs no agent code — the agent is a
/// process in the container and this only scaffolds, streams and enforces. The
/// real `agy`/`claude` invocation is a human-triggered step; tests stand a fake
/// script in for the agent.
pub fn run(
    spec: &CliAgentSpec,
    instance: &dyn Instance,
    events: &dyn Events,
    stop: &AtomicBool,
) -> anyhow::Result<RunOutcome> {
    let limiter = Mutex::new(Limiter::new(spec.rate.clone()));
    let clock = SystemClock;
    run_with(spec, instance, events, stop, &limiter, &clock)
}

pub fn run_with(
    spec: &CliAgentSpec,
    instance: &dyn Instance,
    events: &dyn Events,
    stop: &AtomicBool,
    limiter: &Mutex<Limiter>,
    clock: &dyn Clock,
) -> anyhow::Result<RunOutcome> {
    std::fs::create_dir_all(&spec.workspace)?;
    snap_init(&spec.workspace)?;

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
            "workspace": spec.workspace.display().to_string(),
            "backend": instance.backend(),
            "container": instance.id(),
            "env": spec.env,
        }),
    );

    let mut child = match instance.spawn_streaming(&spec.exec_spec()) {
        Ok(child) => child,
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

    let outcome = pump(spec, instance, events, stop, limiter, clock, &mut child);
    limiter.lock().expect("limiter").release();
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
fn pump(
    spec: &CliAgentSpec,
    instance: &dyn Instance,
    events: &dyn Events,
    stop: &AtomicBool,
    limiter: &Mutex<Limiter>,
    clock: &dyn Clock,
    child: &mut Child,
) -> RunOutcome {
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = stdout.map(|out| {
        let tx = tx.clone();
        std::thread::spawn(move || pipe_lines(out, tx))
    });
    drop(tx);
    let draining = stderr.map(|err| std::thread::spawn(move || drain(err)));

    let started = Instant::now();
    let mut steps = 0u64;
    let mut killed = false;
    let mut halted: Option<String> = None;
    loop {
        match rx.recv_timeout(POLL) {
            Ok(line) => {
                handle_line(spec, events, limiter, clock, &mut steps, &mut halted, &line);
                if halted.is_some() || (spec.budget.max_steps > 0 && steps >= spec.budget.max_steps)
                {
                    let reason = halted
                        .clone()
                        .unwrap_or_else(|| "step budget reached".to_string());
                    halted.get_or_insert(reason);
                    teardown(instance, child);
                    killed = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::Relaxed) {
                    teardown(instance, child);
                    killed = true;
                    break;
                }
                if spec.budget.wall_s > 0 && started.elapsed().as_secs() >= spec.budget.wall_s {
                    halted = Some("wall budget reached".to_string());
                    teardown(instance, child);
                    killed = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if let Some(reader) = reader {
        let _ = reader.join();
    }
    if let Some(draining) = draining {
        let _ = draining.join();
    }
    let status = child.wait().map(status_code).unwrap_or(-1);
    let snap = snap_commit(&spec.workspace, &format!("final step {steps}")).unwrap_or(Value::Null);
    limiter.lock().expect("limiter").record_success();
    if killed {
        events.emit(
            "error",
            json!({"run": spec.run, "reason": halted.clone().unwrap_or_else(|| "killed".to_string()), "killed": true, "snapshot": snap}),
        );
    } else {
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

/// Tear down the run: destroy the container (its PID namespace takes the agent
/// and everything it forked with it — the RFC P5.0-v2 kill switch) and reap the
/// host-side `exec` process so it never lingers as a zombie.
fn teardown(instance: &dyn Instance, child: &mut Child) {
    let _ = instance.destroy();
    let _ = child.kill();
}

#[allow(clippy::too_many_arguments)]
fn handle_line(
    spec: &CliAgentSpec,
    events: &dyn Events,
    limiter: &Mutex<Limiter>,
    clock: &dyn Clock,
    steps: &mut u64,
    halted: &mut Option<String>,
    line: &str,
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
            let snap =
                snap_commit(&spec.workspace, &format!("step {}", *steps)).unwrap_or(Value::Null);
            events.emit(
                "tool-call",
                json!({"run": spec.run, "step": *steps, "name": name, "snapshot": snap}),
            );
        }
        "step" | "turn" => {
            *steps += 1;
            let snap =
                snap_commit(&spec.workspace, &format!("step {}", *steps)).unwrap_or(Value::Null);
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

/// The snapshot repository lives beside the workspace, not inside it, so the
/// worktree the agent writes to never contains the GIT_DIR — the one shape that
/// makes `add_all` reject its own git directory.
fn snap_dir(workspace: &Path) -> PathBuf {
    workspace.parent().unwrap_or(workspace).join("cli-snap.git")
}

fn snap_init(workspace: &Path) -> anyhow::Result<()> {
    let git_dir = snap_dir(workspace);
    if git_dir.exists() {
        return Ok(());
    }
    let mut opts = RepositoryInitOptions::new();
    opts.no_reinit(true);
    opts.bare(false);
    opts.workdir_path(workspace);
    let repo = Repository::init_opts(&git_dir, &opts)?;
    drop(repo);
    Ok(())
}

/// A parentless snapshot commit of the workspace tree — the same reversible,
/// self-contained shape the worker uses, so every step the agent takes is
/// restorable. Best effort: a snapshot failure never fails the run.
fn snap_commit(workspace: &Path, label: &str) -> anyhow::Result<Value> {
    let git_dir = snap_dir(workspace);
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
