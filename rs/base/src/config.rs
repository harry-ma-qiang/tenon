use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Guardian {
    #[serde(default = "interval_ms")]
    pub interval_ms: u64,
    #[serde(default = "failures")]
    pub failures: u32,
    #[serde(default = "probe_timeout_ms")]
    pub probe_timeout_ms: u64,
}

/// Extra probe plugins, signed by being in base's own config: the file lives
/// in `<home>/probes/` and the sha256 here is what base checks it against
/// before the guardian is allowed to run it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Probes {
    #[serde(default)]
    pub extra: Vec<ProbeEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProbeEntry {
    pub file: String,
    #[serde(default)]
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Worker {
    #[serde(default = "worker_boot_timeout_ms")]
    pub boot_timeout_ms: u64,
    #[serde(default = "pull_interval_ms")]
    pub pull_interval_ms: u64,
    #[serde(default = "keep_packs")]
    pub keep_packs: i64,
}

/// RFC section 8's growth control, as the host's own knob. `keep_events` is 0
/// by default: the event log is the version history and a bounded file is a
/// choice, not the default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Retention {
    #[serde(default = "keep_steps")]
    pub keep_steps: i64,
    #[serde(default = "milestone_every")]
    pub milestone_every: i64,
    #[serde(default = "keep_events")]
    pub keep_events: i64,
    #[serde(default = "blob_grace_ms")]
    pub blob_grace_ms: i64,
}

/// Hard rules v1, approval half (RFC section 5): `mode` decides how a gate is
/// resolved (`ask` queues a row for a human, `auto` waves it through, `deny`
/// refuses at once), `timeout_s` is how long a pending row waits before it
/// expires. The gates themselves are host-affecting actions only.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Approvals {
    #[serde(default = "approval_mode")]
    pub mode: String,
    #[serde(default = "approval_timeout_s")]
    pub timeout_s: u64,
    #[serde(default = "spawn_soft_limit")]
    pub spawn_soft_limit: usize,
    #[serde(default)]
    pub gate_config_patch: bool,
    #[serde(default = "enabled")]
    pub gate_snap_export: bool,
    #[serde(default)]
    pub gated_tools: Vec<String>,
}

/// Hard rules v1, budget half: every limit is off at `0`, and every one of
/// them is a hard stop rather than a warning.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct Budgets {
    #[serde(default)]
    pub tokens: i64,
    #[serde(default)]
    pub usd: f64,
    #[serde(default)]
    pub wall_s: u64,
    #[serde(default)]
    pub processes: i64,
}

/// The price table the usd budget needs, in dollars per 1000 tokens.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
pub struct Prices {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
}

/// Human gates per tier of RFC section 10's table, one knob each: `auto`
/// promotes on a green verify, `ask` puts the proposal in the approvals queue
/// first. L0 (the barebone itself) has no row here because it never changes at
/// runtime.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tiers {
    #[serde(default = "tier")]
    pub kernel: String,
    #[serde(default = "tier")]
    pub plugin: String,
    #[serde(default = "tier")]
    pub worker: String,
    #[serde(default = "tier")]
    pub config: String,
}

/// One benchmark task of the promotion gate: a prompt for that env's agent and
/// what its answer has to contain, or which tools it has to have called.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BenchTask {
    pub prompt: String,
    #[serde(default)]
    pub expect_substring: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<String>,
}

/// "Better" made measurable (RFC section 10): the task set a candidate has to
/// score at least as well on as the LKG did, and how much more it may cost.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Benchmark {
    #[serde(default = "bench_model")]
    pub model: String,
    #[serde(default = "bench_tasks")]
    pub tasks: Vec<BenchTask>,
    #[serde(default = "bench_timeout_s")]
    pub timeout_s: u64,
    #[serde(default = "cost_tolerance")]
    pub cost_tolerance: f64,
}

pub use crate::cli_agent_config::{CliAgent, MountCfg};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Envs {
    #[serde(default = "max_total")]
    pub max_total: usize,
    #[serde(default = "max_depth")]
    pub max_depth: u32,
    #[serde(default = "child_ram_mb")]
    pub ram_mb: u64,
}

/// RFC 8c's app-platform ingress (P4.5): how many `/app/<name>` routes an env
/// and the whole host may hold, the lease window base keeps a live route alive
/// within, and the caps the `/app` proxy enforces. `max_per_env` is also how
/// many host ports each sandbox publishes for its apps to bind (the container
/// ports are a fixed span from `18080`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ingress {
    #[serde(default = "ingress_max_per_env")]
    pub max_per_env: usize,
    #[serde(default = "ingress_max_total")]
    pub max_total: usize,
    #[serde(default = "ingress_lease_ttl_ms")]
    pub lease_ttl_ms: i64,
    #[serde(default = "ingress_probe_ms")]
    pub probe_ms: u64,
    #[serde(default = "ingress_body_limit")]
    pub body_limit: usize,
    #[serde(default = "ingress_max_connections")]
    pub max_connections: usize,
}

/// RFC P4.7 triggers + inbound webhook. `hop_cap` is the loop/amplification
/// guard (RFC 8d.3): a trigger drops an envelope whose hop counter would exceed
/// it. `calls_per_min` bounds one `http_post` trigger's outbound rate.
/// `webhook_body_limit` caps a `POST /hook/<topic>` body.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Triggers {
    #[serde(default = "hop_cap")]
    pub hop_cap: u32,
    #[serde(default = "calls_per_min")]
    pub calls_per_min: u32,
    #[serde(default = "webhook_body_limit")]
    pub webhook_body_limit: usize,
    #[serde(default = "http_retries")]
    pub http_retries: u32,
    /// Action kinds that require a human approval before they fire
    /// (`http_post`, `prompt`). A cross-env prompt and an http_post to a new
    /// host are the sensitive cases (RFC 8d.3).
    #[serde(default)]
    pub gated_actions: Vec<String>,
}

fn hop_cap() -> u32 {
    4
}

fn calls_per_min() -> u32 {
    60
}

fn webhook_body_limit() -> usize {
    65_536
}

fn http_retries() -> u32 {
    3
}

impl Default for Triggers {
    fn default() -> Self {
        Self {
            hop_cap: hop_cap(),
            calls_per_min: calls_per_min(),
            webhook_body_limit: webhook_body_limit(),
            http_retries: http_retries(),
            gated_actions: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "root_env")]
    pub root_env: String,
    #[serde(default = "boot_timeout_ms")]
    pub boot_timeout_ms: u64,
    #[serde(default = "stop_grace_ms")]
    pub stop_grace_ms: u64,
    #[serde(default = "request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "sandbox")]
    pub sandbox: String,
    /// The OS user an env's host-side processes run as, when base may change
    /// uid at all. `none` (the default) keeps them base's own user.
    #[serde(default = "env_user")]
    pub env_user: String,
    #[serde(default)]
    pub guardian: Guardian,
    #[serde(default)]
    pub probes: Probes,
    #[serde(default)]
    pub worker: Worker,
    #[serde(default)]
    pub envs: Envs,
    #[serde(default)]
    pub ingress: Ingress,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub approval: Approvals,
    #[serde(default)]
    pub budgets: Budgets,
    #[serde(default)]
    pub usd_per_1k: Prices,
    #[serde(default = "enabled")]
    pub budget_reset_on_reset: bool,
    #[serde(default = "budget_tick_ms")]
    pub budget_tick_ms: u64,
    #[serde(default)]
    pub tiers: Tiers,
    #[serde(default)]
    pub benchmark: Benchmark,
    #[serde(default)]
    pub triggers: Triggers,
    #[serde(default)]
    pub cli_agent: CliAgent,
}

fn root_env() -> String {
    "root".to_string()
}

fn boot_timeout_ms() -> u64 {
    30_000
}

fn stop_grace_ms() -> u64 {
    5_000
}

fn request_timeout_ms() -> u64 {
    10_000
}

fn max_restarts() -> u32 {
    5
}

fn sandbox() -> String {
    "auto".to_string()
}

fn env_user() -> String {
    crate::privilege::NONE.to_string()
}

fn worker_boot_timeout_ms() -> u64 {
    30_000
}

fn pull_interval_ms() -> u64 {
    5_000
}

fn keep_packs() -> i64 {
    40
}

fn keep_steps() -> i64 {
    40
}

fn milestone_every() -> i64 {
    10
}

fn keep_events() -> i64 {
    0
}

fn blob_grace_ms() -> i64 {
    60_000
}

fn max_total() -> usize {
    8
}

fn max_depth() -> u32 {
    3
}

fn child_ram_mb() -> u64 {
    512
}

fn ingress_max_per_env() -> usize {
    4
}

fn ingress_max_total() -> usize {
    32
}

fn ingress_lease_ttl_ms() -> i64 {
    15_000
}

fn ingress_probe_ms() -> u64 {
    1_000
}

fn ingress_body_limit() -> usize {
    1_048_576
}

fn ingress_max_connections() -> usize {
    64
}

fn approval_mode() -> String {
    "ask".to_string()
}

fn approval_timeout_s() -> u64 {
    60
}

fn spawn_soft_limit() -> usize {
    2
}

fn enabled() -> bool {
    true
}

fn budget_tick_ms() -> u64 {
    5_000
}

fn tier() -> String {
    "auto".to_string()
}

fn bench_model() -> String {
    "fake".to_string()
}

fn bench_timeout_s() -> u64 {
    120
}

fn cost_tolerance() -> f64 {
    1.5
}

/// The default set is deliberately tiny and model-independent: one turn that
/// proves the loop answers at all. A host that wants a real gate writes its
/// own tasks into `config.yml`.
fn bench_tasks() -> Vec<BenchTask> {
    vec![BenchTask {
        prompt: "benchmark: reply with the single word tenon-bench-ok".to_string(),
        expect_substring: Some("tenon-bench-ok".to_string()),
        tool_calls: Vec::new(),
    }]
}

impl Default for Tiers {
    fn default() -> Self {
        Self {
            kernel: tier(),
            plugin: tier(),
            worker: tier(),
            config: tier(),
        }
    }
}

impl Default for Benchmark {
    fn default() -> Self {
        Self {
            model: bench_model(),
            tasks: bench_tasks(),
            timeout_s: bench_timeout_s(),
            cost_tolerance: cost_tolerance(),
        }
    }
}

impl Tiers {
    /// The gate in front of one target, `auto` for anything unknown: a target
    /// nobody configured is not a reason to refuse an upgrade.
    pub fn of(&self, target: &str) -> &str {
        match target {
            "kernel" => &self.kernel,
            "plugin" => &self.plugin,
            "worker" => &self.worker,
            "config" => &self.config,
            _ => "auto",
        }
    }
}

fn interval_ms() -> u64 {
    2_000
}

fn failures() -> u32 {
    6
}

fn probe_timeout_ms() -> u64 {
    5_000
}

impl Default for Guardian {
    fn default() -> Self {
        Self {
            interval_ms: interval_ms(),
            failures: failures(),
            probe_timeout_ms: probe_timeout_ms(),
        }
    }
}

impl Default for Worker {
    fn default() -> Self {
        Self {
            boot_timeout_ms: worker_boot_timeout_ms(),
            pull_interval_ms: pull_interval_ms(),
            keep_packs: keep_packs(),
        }
    }
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_steps: keep_steps(),
            milestone_every: milestone_every(),
            keep_events: keep_events(),
            blob_grace_ms: blob_grace_ms(),
        }
    }
}

impl Default for Approvals {
    fn default() -> Self {
        Self {
            mode: approval_mode(),
            timeout_s: approval_timeout_s(),
            spawn_soft_limit: spawn_soft_limit(),
            gate_config_patch: false,
            gate_snap_export: enabled(),
            gated_tools: Vec::new(),
        }
    }
}

impl Default for Envs {
    fn default() -> Self {
        Self {
            max_total: max_total(),
            max_depth: max_depth(),
            ram_mb: child_ram_mb(),
        }
    }
}

impl Default for Ingress {
    fn default() -> Self {
        Self {
            max_per_env: ingress_max_per_env(),
            max_total: ingress_max_total(),
            lease_ttl_ms: ingress_lease_ttl_ms(),
            probe_ms: ingress_probe_ms(),
            body_limit: ingress_body_limit(),
            max_connections: ingress_max_connections(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root_env: root_env(),
            boot_timeout_ms: boot_timeout_ms(),
            stop_grace_ms: stop_grace_ms(),
            request_timeout_ms: request_timeout_ms(),
            max_restarts: max_restarts(),
            sandbox: sandbox(),
            env_user: env_user(),
            guardian: Guardian::default(),
            probes: Probes::default(),
            worker: Worker::default(),
            envs: Envs::default(),
            ingress: Ingress::default(),
            retention: Retention::default(),
            approval: Approvals::default(),
            budgets: Budgets::default(),
            usd_per_1k: Prices::default(),
            budget_reset_on_reset: enabled(),
            budget_tick_ms: budget_tick_ms(),
            tiers: Tiers::default(),
            benchmark: Benchmark::default(),
            triggers: Triggers::default(),
            cli_agent: CliAgent::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Config::default();
            config.write(path)?;
            return Ok(config);
        }
        let body =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        serde_yaml::from_str(&body).with_context(|| format!("parse {}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let body = serde_yaml::to_string(self)?;
        std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}
