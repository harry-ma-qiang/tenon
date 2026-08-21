use crate::cli_agent::{self, CliAgentSpec, Events, RunBudget};
use crate::client::Client;
use crate::config::{CliAgent, Config};
use crate::home::Home;
use crate::preflight::{self, Preflight, PreflightSpec};
use crate::ratelimit::RateConfig;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tenon_sandbox::{Instance, Mount, Policy, Spec};

const GUEST_WORKSPACE: &str = "/workspace";
const GUEST_HOME: &str = "/root";
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(60);

/// Everything `tenon cli-agent run` takes past the tenon home. `rpm` overrides
/// the account limiter's per-minute cap; wall/step budgets are hard stops that
/// tear down the container.
pub struct RunArgs {
    pub task: String,
    pub model: String,
    pub rpm: Option<u32>,
    pub wall_s: u64,
    pub max_calls: u64,
}

/// `tenon cli-agent run`: build the env's OCI sandbox with the RFC P5.0-v2 mount
/// model (cred/session volume, per-env cache, machine-id, read-only base),
/// preflight the agent's auth INSIDE the container, then run the agent as a
/// process in that container with the account rate limiter and the wall/step
/// budget. base runs no agent code; on SIGINT the container is torn down.
pub async fn run(home: Option<PathBuf>, args: RunArgs) -> Result<i32> {
    let home = Home::resolve(home)?;
    let config = Config::load(&home.config_file()).unwrap_or_default();
    let env = config.root_env.clone();
    check_model(&args.model)?;
    let run_id = format!("{}-{}", args.model, tenon_storage::now());
    let base_dir = home.cli_run_dir(&run_id);
    std::fs::create_dir_all(&base_dir)?;

    let instance = match build_instance(&home, &config, &env) {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("tenon cli-agent: cannot create the sandbox ({error}); an OCI backend (podman/docker) is required");
            return Ok(2);
        }
    };
    let agent_env = agent_env(&env, &run_id);

    println!(
        "tenon cli-agent: preflight {} INSIDE {} (zero cost, no paid call)",
        args.model,
        instance.id()
    );
    let pf = do_preflight(instance.as_ref(), &args.model, &agent_env).await?;
    report_preflight(&args.model, &config.cli_agent, &pf);
    if !pf.ok {
        let _ = instance.destroy();
        eprintln!("tenon cli-agent: refusing to spend a paid model call — the container blocks the credential volume; log in inside the session volume and retry");
        return Ok(2);
    }

    let base_up = Client::connect(&home.sock()).await.is_ok();
    let mut rate = RateConfig::default();
    if let Some(rpm) = args.rpm {
        rate.rpm = rpm;
    }
    let spec = CliAgentSpec {
        run: run_id.clone(),
        env: env.clone(),
        cmd: args.model.clone(),
        args: agent_args(&args.model, &args.task),
        workspace: home.workspace_dir(&env),
        guest_cwd: GUEST_WORKSPACE.to_string(),
        agent_env,
        rate,
        budget: RunBudget {
            wall_s: args.wall_s,
            max_steps: args.max_calls,
        },
    };

    let (events, publisher) = event_sink(&home, &env, &run_id, base_up);
    let stop = Arc::new(AtomicBool::new(false));
    let watcher_done = Arc::new(AtomicBool::new(false));
    let stop_watcher =
        spawn_stopfile_watcher(base_dir.join("stop"), stop.clone(), watcher_done.clone());
    write_meta(
        &base_dir,
        json!({
            "run": run_id, "model": args.model, "task": args.task, "env": env,
            "started_ms": tenon_storage::now(), "state": "running",
            "container": instance.id(), "backend": instance.backend(),
            "workspace": home.workspace_dir(&env).display().to_string(), "pid": std::process::id(),
        }),
    )?;

    println!(
        "tenon cli-agent: run {run_id} starting in {} (rpm {}, wall {}s, max-calls {})",
        instance.id(),
        spec.rate.rpm,
        args.wall_s,
        args.max_calls
    );
    let ev = events.clone();
    let st = stop.clone();
    let inst = instance.clone();
    let mut handle =
        tokio::task::spawn_blocking(move || cli_agent::run(&spec, inst.as_ref(), ev.as_ref(), &st));
    let joined = tokio::select! {
        res = &mut handle => res,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\ntenon cli-agent: SIGINT — tearing down the container");
            stop.store(true, Ordering::Relaxed);
            (&mut handle).await
        }
    };

    watcher_done.store(true, Ordering::Relaxed);
    let _ = stop_watcher.join();
    drop(events);
    if let Some(publisher) = publisher {
        let _ = publisher.await;
    }
    let outcome = joined??;
    let _ = instance.destroy();
    let state = if outcome.killed { "killed" } else { "done" };
    write_meta(
        &base_dir,
        json!({
            "run": run_id, "model": args.model, "task": args.task, "env": env,
            "state": state, "status": outcome.status, "steps": outcome.steps,
            "killed": outcome.killed, "halted": outcome.halted,
            "workspace": home.workspace_dir(&env).display().to_string(), "ended_ms": tenon_storage::now(),
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
/// own. Builds the sandbox with the mount model, runs the read-only probes
/// INSIDE it, reports whether the model authenticates in the container, then
/// tears the container down.
pub async fn preflight_cmd(home: Option<PathBuf>, model: String) -> Result<i32> {
    let home = Home::resolve(home)?;
    let config = Config::load(&home.config_file()).unwrap_or_default();
    let env = config.root_env.clone();
    check_model(&model)?;
    let instance = match build_instance(&home, &config, &env) {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("tenon cli-agent: cannot create the sandbox ({error}); an OCI backend (podman/docker) is required");
            return Ok(2);
        }
    };
    let agent_env = agent_env(&env, "preflight");
    let pf = do_preflight(instance.as_ref(), &model, &agent_env).await?;
    let _ = instance.destroy();
    report_preflight(&model, &config.cli_agent, &pf);
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
/// adapter's watcher turns into the kill switch (container teardown) on its next
/// poll.
pub async fn stop(home: Option<PathBuf>, run: String) -> Result<i32> {
    let home = Home::resolve(home)?;
    let base = home.cli_run_dir(&run);
    if !base.is_dir() {
        bail!("no such cli-agent run {run}");
    }
    std::fs::write(base.join("stop"), "stop")?;
    println!("tenon cli-agent: stop requested for {run}");
    Ok(0)
}

/// Build the env's OCI sandbox with the RFC P5.0-v2 mount model: the persistent
/// cred/session volume (RW), the fixed machine-id (RO), the per-env cache (RW)
/// and any read-only base dirs. Forces the oci backend — the sandbox-native
/// design has no host-jail fallback.
fn build_instance(home: &Home, config: &Config, env: &str) -> Result<Arc<dyn Instance>> {
    let session = session_dir(home, &config.cli_agent);
    std::fs::create_dir_all(&session)?;
    let cache = home.cli_cache_dir(env);
    std::fs::create_dir_all(&cache)?;
    ensure_cache_manifest(home, env)?;
    let machine_id = ensure_machine_id(home, env)?;
    std::fs::create_dir_all(home.workspace_dir(env))?;

    let mut mounts = vec![
        Mount {
            host: session,
            guest: config.cli_agent.session_guest.clone(),
            ro: false,
        },
        Mount {
            host: machine_id,
            guest: "/etc/machine-id".to_string(),
            ro: true,
        },
        Mount {
            host: cache,
            guest: config.cli_agent.cache_guest.clone(),
            ro: false,
        },
    ];
    for base in &config.cli_agent.ro_base {
        mounts.push(Mount {
            host: PathBuf::from(&base.host),
            guest: base.guest.clone(),
            ro: true,
        });
    }

    let spec = Spec {
        env: env.to_string(),
        image: config.cli_agent.image.clone(),
        binary: None,
        workspace: home.workspace_dir(env),
        gateway: None,
        env_passthrough: vec![],
        policy: Policy {
            ram_mb: config.cli_agent.ram_mb,
            pids_max: config.cli_agent.pids_max,
            egress: true,
        },
        caps: vec![],
        home_hash: home.hash(),
        base_pid: std::process::id() as i32,
        images: None,
        ingress_ports: Vec::new(),
        mounts,
        hostname: Some(format!("tenon-{env}")),
    };
    let backend = tenon_sandbox::backend("oci")?;
    backend.spawn(&spec)
}

fn session_dir(home: &Home, cli: &CliAgent) -> PathBuf {
    cli.session_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.agy_session_dir())
}

/// Generate the env's machine-id once and reuse it. 32 lowercase hex digits, the
/// systemd `/etc/machine-id` format, so the agent sees one stable machine across
/// runs (RFC P5.0-v2 §10.1).
fn ensure_machine_id(home: &Home, env: &str) -> Result<PathBuf> {
    let path = home.cli_machine_id_file(env);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let id: String = crate::token::generate().chars().take(32).collect();
        std::fs::write(&path, format!("{id}\n"))?;
    }
    Ok(path)
}

/// Write the cache version manifest if absent; on reload a matching manifest
/// means the cache (node_modules/venv/pip) is reused. Minimal policy: the
/// manifest records the tenon version and creation time, and is kept as-is when
/// it already matches.
fn ensure_cache_manifest(home: &Home, env: &str) -> Result<bool> {
    let path = home.cli_cache_manifest(env);
    let version = env!("CARGO_PKG_VERSION");
    if let Ok(body) = std::fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            let matched = value.get("tenon").and_then(Value::as_str) == Some(version);
            return Ok(matched);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let manifest = json!({"tenon": version, "created_ms": tenon_storage::now(), "items": {}});
    std::fs::write(&path, serde_json::to_string_pretty(&manifest)?)?;
    Ok(false)
}

async fn do_preflight(
    instance: &dyn Instance,
    model: &str,
    env: &[(String, String)],
) -> Result<Preflight> {
    let spec = PreflightSpec {
        cmd: model.to_string(),
        probes: probes(model),
        env: env.to_vec(),
        cwd: GUEST_WORKSPACE.to_string(),
        timeout: PREFLIGHT_TIMEOUT,
    };
    preflight::run(instance, &spec)
}

fn report_preflight(model: &str, cli: &CliAgent, pf: &Preflight) {
    if pf.ok {
        println!("tenon cli-agent: preflight CLEAN — {model} authenticated inside the container");
        println!(
            "  cred/session volume mounted read-write at {}",
            cli.session_guest
        );
        return;
    }
    eprintln!(
        "tenon cli-agent: PREFLIGHT FAILED on `{model} {}`",
        pf.probe
    );
    if let Some(signature) = &pf.signature {
        eprintln!("  auth-failure signature: {signature:?}");
    }
    eprintln!(
        "  the container cannot authenticate — log in ONCE inside the session volume ({})",
        cli.session_guest
    );
    eprintln!("  detail: {}", pf.detail);
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

/// The env the agent runs with inside the container. `HOME=/root` is where the
/// cred/session volume is mounted, so agy finds `~/.gemini/antigravity-cli`. The
/// agent uses its OWN native tools on the normal container filesystem.
fn agent_env(env: &str, run: &str) -> Vec<(String, String)> {
    vec![
        ("HOME".to_string(), GUEST_HOME.to_string()),
        (
            "PATH".to_string(),
            "/root/.local/bin:/usr/local/bin:/usr/bin:/bin".to_string(),
        ),
        ("TENON_ENV".to_string(), env.to_string()),
        ("TENON_CLI_RUN".to_string(), run.to_string()),
    ]
}

fn agent_args(model: &str, task: &str) -> Vec<String> {
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
        ],
    }
}

/// The zero-cost preflight probes. `--version` proves the binary starts;
/// `models` is the auth probe — it hits the credential token source and prints
/// "not logged in" when the cred volume is empty, without spending a completion.
fn probes(model: &str) -> Vec<Vec<String>> {
    match model {
        "claude" => vec![vec!["--version".to_string()]],
        _ => vec![vec!["--version".to_string()], vec!["models".to_string()]],
    }
}

fn check_model(model: &str) -> Result<()> {
    match model {
        "agy" | "claude" => Ok(()),
        other => bail!("unknown model {other}, expected agy or claude"),
    }
}

fn write_meta(base: &Path, meta: Value) -> Result<()> {
    std::fs::create_dir_all(base)?;
    std::fs::write(base.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}
