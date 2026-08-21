use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tenon_base::cli_agent::{self, CliAgentSpec, Events, RunBudget};
use tenon_base::preflight::{self, PreflightSpec};
use tenon_base::ratelimit::{Limiter, RateConfig, SystemClock};
use tenon_sandbox::{backend, ExecSpec, Instance, Mount, Policy, Spec};

fn suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[derive(Default)]
struct Collector(Mutex<Vec<(String, Value)>>);

impl Events for Collector {
    fn emit(&self, kind: &str, data: Value) {
        self.0.lock().unwrap().push((kind.to_string(), data));
    }
}

impl Collector {
    fn kinds(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(kind, _)| kind.clone())
            .collect()
    }

    fn has(&self, kind: &str) -> bool {
        self.kinds().iter().any(|k| k == kind)
    }

    fn snapshots(&self) -> usize {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, data)| data.get("snapshot").map(|s| !s.is_null()).unwrap_or(false))
            .count()
    }
}

fn fast_rate() -> RateConfig {
    RateConfig {
        rpm: 600,
        rpd: 0,
        min_gap_ms: 0,
        jitter_ms: 0,
        concurrency: 1,
        ..RateConfig::default()
    }
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tenon-cli-v2-{tag}-{}", suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn agent_env() -> Vec<(String, String)> {
    vec![
        ("HOME".to_string(), "/root".to_string()),
        (
            "PATH".to_string(),
            "/root/.local/bin:/usr/local/bin:/usr/bin:/bin".to_string(),
        ),
    ]
}

struct Layout {
    root: PathBuf,
    workspace: PathBuf,
    cache: PathBuf,
    machine_id: PathBuf,
    ro_base: PathBuf,
    home_hash: String,
}

fn layout(tag: &str) -> Layout {
    let root = tmp(tag);
    let workspace = root.join("workspace");
    let cache = root.join("cache");
    let ro_base = root.join("ro-base");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&ro_base).unwrap();
    let machine_id = root.join("machine-id");
    std::fs::write(&machine_id, "0123456789abcdef0123456789abcdef\n").unwrap();
    Layout {
        root,
        workspace,
        cache,
        machine_id,
        ro_base,
        home_hash: format!("clv2{:x}", suffix() & 0xffff_ffff),
    }
}

fn spawn(layout: &Layout) -> anyhow::Result<std::sync::Arc<dyn Instance>> {
    let sandbox = backend("oci")?;
    let spec = Spec {
        env: "root".to_string(),
        image: None,
        binary: None,
        workspace: layout.workspace.clone(),
        gateway: None,
        env_passthrough: vec![],
        policy: Policy {
            ram_mb: 1024,
            pids_max: 256,
            egress: true,
        },
        caps: vec![],
        home_hash: layout.home_hash.clone(),
        base_pid: std::process::id() as i32,
        images: None,
        ingress_ports: Vec::new(),
        mounts: vec![
            Mount {
                host: layout.cache.clone(),
                guest: "/root/.cache".to_string(),
                ro: false,
            },
            Mount {
                host: layout.machine_id.clone(),
                guest: "/etc/machine-id".to_string(),
                ro: true,
            },
            Mount {
                host: layout.ro_base.clone(),
                guest: "/opt/tenon-base".to_string(),
                ro: true,
            },
        ],
        hostname: Some("tenon-root".to_string()),
    };
    sandbox.spawn(&spec)
}

fn container_gone(cli: &str, home_hash: &str) -> bool {
    let out = std::process::Command::new(cli)
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label=tenon.home={home_hash}"),
            "--format",
            "{{.ID}}",
        ])
        .output();
    match out {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        Err(_) => true,
    }
}

fn cli() -> Option<&'static str> {
    let path = std::env::var_os("PATH")?;
    ["podman", "docker"]
        .into_iter()
        .find(|name| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// The core P5.0-v2a gate: a fake agent inside the sandbox writes a file in its
/// cwd; the edit lands in the SANDBOX workspace (host-visible via the bind), a
/// canary in ~/workspace is untouched (never mounted), the workspace snapshot
/// captures the change, teardown is clean, and the cache + RO base survive a
/// container recreate.
#[test]
fn fake_agent_edits_land_in_the_sandbox_workspace_not_host() {
    let Some(cli) = cli() else {
        println!("skipping: no podman/docker on PATH");
        return;
    };
    let lay = layout("edits");

    let canary_dir = dirs_home()
        .join("workspace")
        .join(format!(".tenon-canary-{}", suffix()));
    std::fs::create_dir_all(&canary_dir).unwrap();
    let canary = canary_dir.join("canary.txt");
    std::fs::write(&canary, "do-not-touch").unwrap();

    // Seed the cache and RO base so the recreate check has something to find.
    std::fs::write(lay.cache.join("dep.txt"), "cached-node-modules").unwrap();
    std::fs::write(lay.ro_base.join("toolchain.txt"), "ro-base-present").unwrap();

    let instance = match spawn(&lay) {
        Ok(instance) => instance,
        Err(error) => {
            println!("skipping: cannot spawn oci instance: {error}");
            let _ = std::fs::remove_dir_all(&canary_dir);
            return;
        }
    };

    let agent = lay.workspace.join("agent.sh");
    write_exec(
        &agent,
        "echo '{\"type\":\"text\",\"text\":\"hello\"}'\n\
         echo agent-was-here > from-agent.txt\n\
         echo '{\"type\":\"tool_use\",\"name\":\"write\"}'\n\
         cat /etc/machine-id\n\
         echo '{\"type\":\"text\",\"text\":\"done\"}'",
    );

    let spec = CliAgentSpec {
        run: "edits".to_string(),
        env: "root".to_string(),
        cmd: "sh".to_string(),
        args: vec!["/workspace/agent.sh".to_string()],
        workspace: lay.workspace.clone(),
        guest_cwd: "/workspace".to_string(),
        agent_env: agent_env(),
        rate: fast_rate(),
        budget: RunBudget::default(),
    };
    let events = Collector::default();
    let stop = AtomicBool::new(false);
    let outcome = cli_agent::run(&spec, instance.as_ref(), &events, &stop).expect("run");

    assert!(events.has("started"), "kinds: {:?}", events.kinds());
    assert!(events.has("tool-call"), "kinds: {:?}", events.kinds());
    assert!(events.has("done"), "kinds: {:?}", events.kinds());
    assert!(!outcome.killed);

    let landed = lay.workspace.join("from-agent.txt");
    assert!(
        landed.is_file(),
        "the agent's file must land in the sandbox workspace"
    );
    assert_eq!(
        std::fs::read_to_string(&landed).unwrap().trim(),
        "agent-was-here"
    );
    assert!(
        events.snapshots() >= 2,
        "expected per-step + final workspace snapshots, got {}",
        events.snapshots()
    );

    // Isolation: the ~/workspace canary is byte-for-byte unchanged.
    assert_eq!(
        std::fs::read_to_string(&canary).unwrap(),
        "do-not-touch",
        "~/workspace canary must be untouched — it is never mounted"
    );

    // Teardown is clean.
    instance.destroy().expect("destroy");
    drop(instance);
    assert!(
        container_gone(cli, &lay.home_hash),
        "teardown must leave no container"
    );

    // Recreate: cache + RO base persist across a fresh container.
    let again = spawn(&lay).expect("recreate");
    let dep = again
        .exec(
            "sh",
            &["-c".to_string(), "cat /root/.cache/dep.txt".to_string()],
            Duration::from_secs(15),
        )
        .expect("exec cache read");
    assert!(
        String::from_utf8_lossy(&dep.stdout).contains("cached-node-modules"),
        "cache must survive a recreate: {dep:?}"
    );
    let tool = again
        .exec(
            "sh",
            &[
                "-c".to_string(),
                "cat /opt/tenon-base/toolchain.txt".to_string(),
            ],
            Duration::from_secs(15),
        )
        .expect("exec ro-base read");
    assert!(
        String::from_utf8_lossy(&tool.stdout).contains("ro-base-present"),
        "RO base must survive a recreate: {tool:?}"
    );
    again.destroy().expect("destroy recreate");

    let _ = std::fs::remove_dir_all(&canary_dir);
    let _ = std::fs::remove_dir_all(&lay.root);
}

/// The preflight-failure path: a fake `agy` on the container PATH prints
/// "license expired", so the in-sandbox preflight refuses the run before any
/// paid call.
#[test]
fn preflight_refuses_when_the_agent_cannot_authenticate() {
    if cli().is_none() {
        println!("skipping: no podman/docker on PATH");
        return;
    }
    let lay = layout("preflight");
    // A fake agy on the agent PATH (/root/.local/bin) that fails auth.
    let bin = lay.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_exec(
        &bin.join("agy"),
        "echo 'Error: session token expired'\nexit 1",
    );

    let sandbox = backend("oci").unwrap();
    let spec = Spec {
        env: "root".to_string(),
        image: None,
        binary: None,
        workspace: lay.workspace.clone(),
        gateway: None,
        env_passthrough: vec![],
        policy: Policy::default(),
        caps: vec![],
        home_hash: lay.home_hash.clone(),
        base_pid: std::process::id() as i32,
        images: None,
        ingress_ports: Vec::new(),
        mounts: vec![Mount {
            host: bin.clone(),
            guest: "/root/.local/bin".to_string(),
            ro: true,
        }],
        hostname: Some("tenon-root".to_string()),
    };
    let instance = match sandbox.spawn(&spec) {
        Ok(instance) => instance,
        Err(error) => {
            println!("skipping: cannot spawn oci instance: {error}");
            return;
        }
    };
    let pf_spec = PreflightSpec {
        cmd: "agy".to_string(),
        probes: vec![vec!["--version".to_string()]],
        env: agent_env(),
        cwd: "/workspace".to_string(),
        timeout: Duration::from_secs(30),
    };
    let pf = preflight::run(instance.as_ref(), &pf_spec).expect("preflight");
    instance.destroy().expect("destroy");
    assert!(!pf.ok, "preflight must fail on an auth-failure signature");
    assert_eq!(pf.signature.as_deref(), Some("expired"));

    let _ = std::fs::remove_dir_all(&lay.root);
}

/// The account rate limiter gates the run: a limiter already halted by its
/// circuit breaker refuses the run before the agent is ever spawned in the
/// container. No container needed — the gate is upstream of the spawn.
#[test]
fn a_halted_rate_limiter_refuses_the_run() {
    let lay = layout("rate");
    let config = RateConfig {
        rpm: 6,
        min_gap_ms: 0,
        jitter_ms: 0,
        breaker_threshold: 1,
        breaker_max_opens: 1,
        ..RateConfig::default()
    };
    let mut limiter = Limiter::new(config.clone());
    assert!(limiter.record(0, true).halted, "breaker should halt");
    let limiter = Mutex::new(limiter);
    let clock = SystemClock;

    let spec = CliAgentSpec {
        run: "rate".to_string(),
        env: "root".to_string(),
        cmd: "sh".to_string(),
        args: vec!["-c".to_string(), "true".to_string()],
        workspace: lay.workspace.clone(),
        guest_cwd: "/workspace".to_string(),
        agent_env: agent_env(),
        rate: config,
        budget: RunBudget::default(),
    };
    let events = Collector::default();
    let stop = AtomicBool::new(false);
    let outcome =
        cli_agent::run_with(&spec, &Unreachable, &events, &stop, &limiter, &clock).expect("run");
    assert!(
        outcome.halted.is_some(),
        "a halted limiter must halt the run"
    );
    assert!(events.has("error"));

    let _ = std::fs::remove_dir_all(&lay.root);
}

/// An instance whose `spawn_streaming` must never be called — used to prove the
/// rate limiter gate is upstream of touching the container at all.
struct Unreachable;

impl Instance for Unreachable {
    fn id(&self) -> &str {
        "unreachable"
    }
    fn backend(&self) -> &'static str {
        "none"
    }
    fn attach_addr(&self) -> tenon_sandbox::Endpoint {
        tenon_sandbox::Endpoint::Direct
    }
    fn workspace_path(&self) -> String {
        "/workspace".to_string()
    }
    fn binary_path(&self) -> String {
        "/usr/local/bin/tenon".to_string()
    }
    fn exec(
        &self,
        _cmd: &str,
        _args: &[String],
        _timeout: Duration,
    ) -> anyhow::Result<tenon_sandbox::ExecOutcome> {
        panic!("exec must not be reached when the limiter has halted");
    }
    fn spawn_streaming(&self, _spec: &ExecSpec) -> anyhow::Result<std::process::Child> {
        panic!("spawn_streaming must not be reached when the limiter has halted");
    }
    fn destroy(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}
