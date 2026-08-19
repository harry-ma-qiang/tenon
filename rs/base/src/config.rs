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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Envs {
    #[serde(default = "max_total")]
    pub max_total: usize,
    #[serde(default = "max_depth")]
    pub max_depth: u32,
    #[serde(default = "child_ram_mb")]
    pub ram_mb: u64,
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
            retention: Retention::default(),
            approval: Approvals::default(),
            budgets: Budgets::default(),
            usd_per_1k: Prices::default(),
            budget_reset_on_reset: enabled(),
            budget_tick_ms: budget_tick_ms(),
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
