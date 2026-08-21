use serde::{Deserialize, Serialize};

/// RFC P5.0-v2 sandbox-native cli-agent knobs. The agent runs INSIDE the env's
/// OCI sandbox with a mount model, not a host jail. `image` is the container
/// image (the human sets one carrying agy/node, or leaves it and mounts the host
/// toolchain read-only via `ro_base`). `session_dir` is the persistent host
/// cred/session volume, mounted read-write at `session_guest` inside — the human
/// logs in there ONCE and every run reuses it; the host's real `~/.gemini` is
/// never mounted. `cache_guest` is where the per-env cache volume lands (npm,
/// pip, venv). `ro_base` is a list of host dirs mounted read-only (toolchains,
/// DSH). `ram_mb`/`pids_max` are the container resource caps.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CliAgent {
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub session_dir: Option<String>,
    #[serde(default = "session_guest")]
    pub session_guest: String,
    #[serde(default = "cache_guest")]
    pub cache_guest: String,
    #[serde(default)]
    pub ro_base: Vec<MountCfg>,
    #[serde(default = "cli_ram_mb")]
    pub ram_mb: u64,
    #[serde(default = "cli_pids_max")]
    pub pids_max: u64,
}

/// One read-only base mount: a host directory exposed at a guest path (RFC
/// P5.0-v2 §10.1). The default list is empty; the plumbing is wired now for DSH
/// and toolchains later.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountCfg {
    pub host: String,
    pub guest: String,
}

fn session_guest() -> String {
    "/root/.gemini/antigravity-cli".to_string()
}

fn cache_guest() -> String {
    "/root/.cache".to_string()
}

fn cli_ram_mb() -> u64 {
    2048
}

fn cli_pids_max() -> u64 {
    512
}

impl Default for CliAgent {
    fn default() -> Self {
        Self {
            image: None,
            session_dir: None,
            session_guest: session_guest(),
            cache_guest: cache_guest(),
            ro_base: Vec::new(),
            ram_mb: cli_ram_mb(),
            pids_max: cli_pids_max(),
        }
    }
}
