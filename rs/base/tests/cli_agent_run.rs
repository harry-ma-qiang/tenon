use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tenon_base::cli_agent::{self, CliAgentSpec, Events, RunBudget};
use tenon_base::jail::Limits;
use tenon_base::preflight::{self, PreflightSpec};
use tenon_base::ratelimit::{Limiter, RateConfig, SystemClock};

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
    fn has(&self, kind: &str) -> bool {
        self.0.lock().unwrap().iter().any(|(k, _)| k == kind)
    }
}

fn loose_limits() -> Limits {
    Limits {
        nproc: 0,
        mem_bytes: 0,
        cpu_secs: 0,
        nofile: 0,
        mem_max: 0,
        pids_max: 0,
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

fn write_agent(body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tenon-fake-agent-{}", suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agent.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn home(run: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tenon-cli-run-{run}-{}", suffix()))
}

fn scratch_dir(run: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = home(run);
    let scratch = root.join("cli").join(run).join("scratch");
    let tmp = root.join("cli").join(run).join("tmp");
    (root, scratch, tmp)
}

/// The scratch disk cap: a fake agent that fills scratch past the 1 MB ceiling is
/// SIGKILLed by the watcher, and the run reports `killed` with a `violation`
/// event — so an overnight agent cannot fill the host disk.
#[test]
fn scratch_cap_watcher_kills_the_jail() {
    let agent = write_agent(
        "head -c 3000000 </dev/zero >big.dat 2>/dev/null\n\
         echo '{\"type\":\"text\",\"text\":\"filled\"}'\n\
         sleep 30",
    );
    let run = "capfill";
    let root = home(run);
    let spec = CliAgentSpec {
        run: run.to_string(),
        env: "root".to_string(),
        cmd: agent.display().to_string(),
        args: vec![],
        root: root.clone(),
        mcp: None,
        ro_allow: vec![],
        rw_state: vec![],
        limits: loose_limits(),
        rate: fast_rate(),
        budget: RunBudget::default(),
        extra_env: vec![],
        cgroup_parent: None,
        agent_home: root.clone(),
        scratch_max_mb: 1,
    };
    let events = Collector::default();
    let stop = AtomicBool::new(false);
    let limiter = Mutex::new(Limiter::new(spec.rate.clone()));
    let clock = SystemClock;

    let started = Instant::now();
    let outcome = cli_agent::run_with(&spec, &events, &stop, &limiter, &clock).expect("run");

    assert!(outcome.killed, "scratch overflow must kill the run");
    assert!(
        events.has("violation"),
        "a disk-cap violation must be emitted"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the cap kill should be prompt, took {:?}",
        started.elapsed()
    );
    assert!(
        outcome
            .halted
            .as_deref()
            .unwrap_or_default()
            .contains("scratch disk cap"),
        "halt reason names the cap: {:?}",
        outcome.halted
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(agent.parent().unwrap());
}

/// The auth preflight refuses when a probe prints an auth-failure signature: a
/// fake agent standing in for a jail-blocked-cred `agy` prints "license expired"
/// and the preflight fails, naming the signature, before any paid call.
#[test]
fn preflight_refuses_on_auth_signature() {
    let agent = write_agent("echo 'Error: license expired, please login'\nexit 0");
    let (root, scratch, tmp) = scratch_dir("pf-fail");
    let spec = PreflightSpec {
        cmd: agent.display().to_string(),
        probes: vec![vec!["--version".to_string()]],
        scratch,
        tmp,
        agent_home: root.clone(),
        rw_allow: vec![],
        ro_allow: vec![],
        limits: loose_limits(),
        env: vec![("HOME".to_string(), root.display().to_string())],
    };
    let pf = preflight::run(&spec).expect("preflight");
    assert!(!pf.ok, "auth signature must fail the preflight");
    assert_eq!(pf.signature.as_deref(), Some("license"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(agent.parent().unwrap());
}

/// A clean probe (version-like output, zero exit, no signature) clears the
/// preflight, the only state that allows a paid run.
#[test]
fn preflight_passes_on_clean_output() {
    let agent = write_agent("echo 'agy version 1.1.17'\nexit 0");
    let (root, scratch, tmp) = scratch_dir("pf-ok");
    let spec = PreflightSpec {
        cmd: agent.display().to_string(),
        probes: vec![
            vec!["--version".to_string()],
            vec!["mcp".to_string(), "list".to_string()],
        ],
        scratch,
        tmp,
        agent_home: root.clone(),
        rw_allow: vec![],
        ro_allow: vec![],
        limits: loose_limits(),
        env: vec![("HOME".to_string(), root.display().to_string())],
    };
    let pf = preflight::run(&spec).expect("preflight");
    assert!(pf.ok, "clean output must pass: {}", pf.detail);
    assert!(pf.signature.is_none());
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(agent.parent().unwrap());
}

/// A non-zero exit from a probe fails the preflight even without an auth
/// signature: a broken agent binary is not a green light for a paid run.
#[test]
fn preflight_refuses_on_nonzero_exit() {
    let agent = write_agent("echo 'boom' 1>&2\nexit 3");
    let (root, scratch, tmp) = scratch_dir("pf-exit");
    let spec = PreflightSpec {
        cmd: agent.display().to_string(),
        probes: vec![vec!["--version".to_string()]],
        scratch,
        tmp,
        agent_home: root.clone(),
        rw_allow: vec![],
        ro_allow: vec![],
        limits: loose_limits(),
        env: vec![("HOME".to_string(), root.display().to_string())],
    };
    let pf = preflight::run(&spec).expect("preflight");
    assert!(!pf.ok, "a non-zero probe exit must fail the preflight");
    assert!(pf.detail.contains("exit 3"), "detail: {}", pf.detail);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(agent.parent().unwrap());
}
