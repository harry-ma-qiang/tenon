pub mod krun;
mod landlock;
mod none;
mod oci;
mod proc;

pub use none::NoSandbox;

use anyhow::{bail, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize)]
pub struct Policy {
    pub ram_mb: u64,
    pub pids_max: u64,
    pub egress: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            ram_mb: 512,
            pids_max: 256,
            egress: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Spec {
    pub env: String,
    pub image: Option<String>,
    pub binary: Option<PathBuf>,
    pub workspace: PathBuf,
    pub gateway: Option<String>,
    pub env_passthrough: Vec<String>,
    pub policy: Policy,
    pub caps: Vec<String>,
    pub home_hash: String,
    pub base_pid: i32,
    /// Where a backend that needs a prepared root filesystem looks for one:
    /// `<images>/<image>/rootfs`. Only krun reads it; oci pulls by reference
    /// and landlock has no root of its own.
    pub images: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Endpoint {
    Direct,
    Uds(PathBuf),
    Tcp(String, u16),
}

#[derive(Debug, Clone, Default)]
pub struct ExecOutcome {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

pub trait Instance: Send + Sync {
    fn id(&self) -> &str;
    fn backend(&self) -> &'static str;
    fn attach_addr(&self) -> Endpoint;

    /// Where the workspace and the `tenon` binary are found *inside* the
    /// instance. They differ from the host paths only where the backend
    /// relocates them (oci does, landlock and none do not), and the worker's
    /// launch line and every handle path base hands back are written in these.
    fn workspace_path(&self) -> String;
    fn binary_path(&self) -> String;
    fn exec(&self, cmd: &str, args: &[String], timeout: Duration) -> Result<ExecOutcome>;
    fn destroy(&self) -> Result<()>;

    /// Starts the resident worker inside the instance and reports whether the
    /// backend took the job. `false` — every backend that can `exec` into a
    /// live instance — leaves base to run its own launch line. `true` means the
    /// worker is the instance's init and base must not try to start it again;
    /// a VM backend has no exec, so the boot of the guest *is* the boot of the
    /// worker and it can only happen once the gateway is listening.
    fn start_worker(&self, _env: &str, _gateway: &str) -> Result<bool> {
        Ok(false)
    }
}

pub trait Sandbox: Send + Sync {
    fn backend(&self) -> &'static str;
    fn spawn(&self, spec: &Spec) -> Result<Arc<dyn Instance>>;

    /// Remove containers left over from a dead base of this home. `all` skips the
    /// liveness check and removes every match regardless of whether its owning base
    /// pid is still alive; returns the number reaped.
    fn reap(&self, _home_hash: &str, _all: bool) -> Result<usize> {
        Ok(0)
    }

    /// The gateway address this backend needs for `env`, given the address base
    /// would use on its own (a unix socket in the env's gateway directory).
    /// `None` keeps that default. A VM backend answers with a `tcp:` address
    /// because a host unix socket is not a path the guest has.
    fn gateway_address(&self, _env: &str, _default: &str) -> Option<String> {
        None
    }
}

pub struct Skip {
    pub backend: &'static str,
    pub reason: String,
}

pub struct Detected {
    pub sandbox: Box<dyn Sandbox>,
    pub skipped: Vec<Skip>,
}

pub fn host_binary(spec: &Spec) -> String {
    spec.binary
        .clone()
        .or_else(|| std::env::current_exe().ok())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "tenon".to_string())
}

pub fn gateway_dir(address: &str) -> Option<PathBuf> {
    address
        .strip_prefix("unix:")
        .map(PathBuf::from)
        .and_then(|path| path.parent().map(|dir| dir.to_path_buf()))
}

pub fn detect() -> Detected {
    let mut skipped = Vec::new();
    match krun::probe() {
        Ok(sandbox) => {
            return Detected { sandbox, skipped };
        }
        Err(reason) => skipped.push(Skip {
            backend: "krun",
            reason,
        }),
    }
    match oci::probe() {
        Ok(sandbox) => {
            return Detected { sandbox, skipped };
        }
        Err(reason) => skipped.push(Skip {
            backend: "oci",
            reason,
        }),
    }
    match landlock::probe() {
        Ok(sandbox) => {
            return Detected { sandbox, skipped };
        }
        Err(reason) => skipped.push(Skip {
            backend: "landlock",
            reason,
        }),
    }
    skipped.push(Skip {
        backend: "none",
        reason: "explicit fallback, no isolation".to_string(),
    });
    Detected {
        sandbox: Box::new(NoSandbox),
        skipped,
    }
}

pub fn backend(name: &str) -> Result<Box<dyn Sandbox>> {
    match name {
        "none" => Ok(Box::new(NoSandbox)),
        "auto" => Ok(detect().sandbox),
        "oci" => oci::probe().map_err(|reason| anyhow::anyhow!(reason)),
        "landlock" => landlock::probe().map_err(|reason| anyhow::anyhow!(reason)),
        "krun" => krun::probe().map_err(|reason| anyhow::anyhow!(reason)),
        other => bail!("unknown sandbox backend {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            env: "root".to_string(),
            image: None,
            binary: None,
            workspace: PathBuf::from("/tmp/tenon-workspace"),
            gateway: None,
            env_passthrough: vec![],
            policy: Policy::default(),
            caps: vec![],
            home_hash: "deadbeef0000".to_string(),
            base_pid: std::process::id() as i32,
            images: None,
        }
    }

    #[test]
    fn the_none_backend_hands_back_a_direct_instance() {
        let sandbox = backend("none").unwrap();
        let instance = sandbox.spawn(&spec()).unwrap();
        assert_eq!(instance.backend(), "none");
        assert_eq!(instance.attach_addr(), Endpoint::Direct);
        assert_eq!(instance.id(), "none:root");
        assert_eq!(instance.workspace_path(), "/tmp/tenon-workspace");
        instance.destroy().unwrap();
    }

    #[test]
    fn an_unknown_backend_name_is_rejected() {
        assert!(backend("qemu").is_err());
    }

    #[test]
    fn krun_always_names_a_reason_here() {
        let reason = krun::probe()
            .err()
            .expect("no hypervisor and no libkrun on this box");
        assert!(reason.starts_with("krun unavailable: "), "{reason}");
    }

    #[test]
    fn detection_reports_the_krun_reason_it_skipped_on() {
        let detected = detect();
        if detected.sandbox.backend() == "krun" {
            return;
        }
        let skip = detected
            .skipped
            .iter()
            .find(|skip| skip.backend == "krun")
            .expect("krun is probed first, so it is either chosen or skipped with a reason");
        assert_eq!(Some(skip.reason.clone()), krun::unavailable());
    }

    #[test]
    fn only_a_vm_backend_moves_the_gateway_off_its_unix_socket() {
        let default = "unix:/home/x/.tenon/run/gw-root/gateway.sock";
        assert_eq!(NoSandbox.gateway_address("root", default), None);
        assert_eq!(
            krun::Krun.gateway_address("root", default),
            Some(krun::gateway_address("root"))
        );
    }

    #[test]
    fn gateway_dir_extracts_the_unix_socket_directory() {
        assert_eq!(
            gateway_dir("unix:/home/x/.tenon/run/gateway-root.sock"),
            Some(PathBuf::from("/home/x/.tenon/run"))
        );
        assert_eq!(gateway_dir("tcp:127.0.0.1:9000"), None);
    }

    #[test]
    fn detect_always_returns_something_and_explains_the_rest() {
        let detected = detect();
        assert!(!detected.skipped.is_empty() || detected.sandbox.backend() == "krun");
    }
}
