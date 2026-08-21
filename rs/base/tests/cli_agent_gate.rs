use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tenon_base::cli_agent::{self, CliAgentSpec, Events, McpEndpoint, RunBudget};
use tenon_base::jail::Limits;
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
    std::env::temp_dir().join(format!("tenon-cli-home-{run}-{}", suffix()))
}

/// The adapter, driven by a fake agent that emits a few JSON lines and exits,
/// produces the expected bus events, writes the MCP config, and git-snaps each
/// step. No real model is touched.
#[test]
fn fake_agent_produces_events_snapshots_and_mcp_config() {
    let agent = write_agent(
        "echo '{\"type\":\"text\",\"text\":\"hello\"}'\n\
         echo scratch-file > note.txt\n\
         echo '{\"type\":\"tool_use\",\"name\":\"bash\"}'\n\
         echo '{\"type\":\"text\",\"text\":\"done\"}'",
    );
    let run = "adapter1";
    let root = home(run);
    let spec = CliAgentSpec {
        run: run.to_string(),
        env: "root".to_string(),
        cmd: agent.display().to_string(),
        args: vec![],
        root: root.clone(),
        mcp: Some(McpEndpoint {
            url: "http://127.0.0.1:38080/mcp".to_string(),
            token: "test-token".to_string(),
        }),
        ro_allow: vec![],
        rw_state: vec![],
        limits: loose_limits(),
        rate: fast_rate(),
        budget: RunBudget::default(),
        extra_env: vec![],
        cgroup_parent: None,
        agent_home: root.clone(),
        scratch_max_mb: 0,
    };
    let events = Collector::default();
    let stop = AtomicBool::new(false);
    let limiter = Mutex::new(Limiter::new(spec.rate.clone()));
    let clock = SystemClock;
    let outcome = cli_agent::run_with(&spec, &events, &stop, &limiter, &clock).expect("run");

    assert!(events.has("started"), "kinds: {:?}", events.kinds());
    assert!(events.has("tool-call"), "kinds: {:?}", events.kinds());
    assert!(events.has("output"));
    assert!(events.has("done"));
    assert_eq!(outcome.steps, 1, "one tool_use is one step");
    assert!(!outcome.killed);
    assert!(
        events.snapshots() >= 2,
        "expected per-step + final git-snaps"
    );

    let mcp = spec.scratch().join(".mcp.json");
    let body: Value = serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
    assert_eq!(body["mcpServers"]["tenon"]["type"], json!("http"));
    assert_eq!(
        body["mcpServers"]["tenon"]["url"],
        json!("http://127.0.0.1:38080/mcp")
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(agent.parent().unwrap());
}

/// Kill switch: a long-running agent is SIGKILLed the moment `stop` is set; the
/// run ends promptly as `killed` with an error event.
#[test]
fn respects_kill() {
    let agent = write_agent("echo '{\"type\":\"text\",\"text\":\"starting\"}'\nsleep 30");
    let run = "killme";
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
        scratch_max_mb: 0,
    };
    let events = Collector::default();
    let stop = AtomicBool::new(false);
    let limiter = Mutex::new(Limiter::new(spec.rate.clone()));
    let clock = SystemClock;

    let started = Instant::now();
    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(400));
            stop.store(true, Ordering::Relaxed);
        });
        cli_agent::run_with(&spec, &events, &stop, &limiter, &clock).expect("run")
    });

    assert!(outcome.killed, "should report killed");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "kill should be prompt, took {:?}",
        started.elapsed()
    );
    assert!(events.has("error"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(agent.parent().unwrap());
}

/// Budget: a step ceiling SIGKILLs the jail once the agent reports that many
/// steps, even though the fake agent would keep going.
#[test]
fn step_budget_halts_the_run() {
    let agent = write_agent(
        "for i in 1 2 3 4 5; do echo '{\"type\":\"tool_use\",\"name\":\"bash\"}'; sleep 0.2; done\n\
         sleep 5",
    );
    let run = "budget";
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
        budget: RunBudget {
            wall_s: 0,
            max_steps: 2,
        },
        extra_env: vec![],
        cgroup_parent: None,
        agent_home: root.clone(),
        scratch_max_mb: 0,
    };
    let events = Collector::default();
    let stop = AtomicBool::new(false);
    let limiter = Mutex::new(Limiter::new(spec.rate.clone()));
    let clock = SystemClock;
    let outcome = cli_agent::run_with(&spec, &events, &stop, &limiter, &clock).expect("run");
    assert!(outcome.killed);
    assert_eq!(outcome.steps, 2, "stopped at the step ceiling");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(agent.parent().unwrap());
}
