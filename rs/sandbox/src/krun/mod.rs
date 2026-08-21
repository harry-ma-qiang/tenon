mod ffi;
pub mod image;
pub mod vmm;

use crate::{proc, Endpoint, ExecOutcome, Instance, Sandbox, Spec};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wait_timeout::ChildExt;

pub use vmm::Config as VmmConfig;

const GUEST_WORKSPACE: &str = "/workspace";
const GUEST_BINARY: &str = "/usr/local/bin/tenon";
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const DEFAULT_IMAGE: &str = "default";
const DEFAULT_GATEWAY_PORT: u16 = 10000;
/// How far above the base port a per-env offset may reach. Deterministic, so a
/// restarted env keeps the port its gateway is already listening on.
const GATEWAY_PORT_SPAN: u16 = 512;
const RLIMIT_NPROC: u32 = 6;
const STOP_GRACE: Duration = Duration::from_secs(3);
const SMOKE_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Krun;

pub struct KrunInstance {
    id: String,
    env: String,
    rootfs: PathBuf,
    workspace: PathBuf,
    config_file: PathBuf,
    log: PathBuf,
    console: PathBuf,
    binary: PathBuf,
    ram_mb: u32,
    pids_max: u64,
    passthrough: Vec<(String, String)>,
    port: u16,
    child: Mutex<Option<Child>>,
    destroyed: AtomicBool,
}

/// `Ok` only when this host can actually run a microVM: the hypervisor is
/// reachable **and** libkrun resolves. Every other answer names both halves, so
/// `tenon status` explains what is missing rather than that something is.
pub fn probe() -> Result<Box<dyn Sandbox>, String> {
    match unavailable() {
        None => Ok(Box::new(Krun)),
        Some(reason) => Err(reason),
    }
}

pub fn unavailable() -> Option<String> {
    let mut reasons = Vec::new();
    if let Err(reason) = hypervisor() {
        reasons.push(reason);
    }
    if let Err(reason) = ffi::load() {
        reasons.push(reason);
    }
    match reasons.is_empty() {
        true => None,
        false => Some(format!("krun unavailable: {}", reasons.join("; "))),
    }
}

#[cfg(target_os = "linux")]
fn hypervisor() -> Result<(), String> {
    let path = Path::new("/dev/kvm");
    if !path.exists() {
        return Err("/dev/kvm absent (no hardware virtualisation on this host)".to_string());
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(|_| ())
        .map_err(|error| format!("/dev/kvm not usable: {error}"))
}

#[cfg(target_os = "macos")]
fn hypervisor() -> Result<(), String> {
    let mut command = std::process::Command::new("sysctl");
    command.args(["-n", "kern.hv_support"]);
    let outcome = proc::run(command, Duration::from_secs(5))
        .map_err(|error| format!("kern.hv_support unreadable: {error}"))?;
    match String::from_utf8_lossy(&outcome.stdout).trim() {
        "1" => Ok(()),
        other => Err(format!("HVF unavailable (kern.hv_support={other})")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn hypervisor() -> Result<(), String> {
    Err("krun runs on Linux (KVM) and macOS (HVF) only".to_string())
}

/// The C API and the symbols this backend drives, for `tenon status` and the
/// docs to quote rather than restate.
pub fn api() -> (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
) {
    (ffi::API, ffi::REQUIRED, ffi::OPTIONAL)
}

/// A stable, per-env TCP port on the host loopback. The gateway is a host
/// process either way; under krun the guest reaches it through TSI, which
/// forwards guest socket calls to the host network stack, so the address the
/// guest dials is the address the host listens on.
pub fn gateway_port(env: &str) -> u16 {
    let base = std::env::var("TENON_KRUN_GATEWAY_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(DEFAULT_GATEWAY_PORT);
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in env.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    base.saturating_add((hash % GATEWAY_PORT_SPAN as u64) as u16)
}

pub fn gateway_address(env: &str) -> String {
    format!("tcp:127.0.0.1:{}", gateway_port(env))
}

fn images_dir(spec: &Spec) -> PathBuf {
    spec.images.clone().unwrap_or_else(image::default_dir)
}

/// `<images>/<name>/rootfs`, or the path itself when the image is one. The
/// error is the command that fixes it: nothing here ever pulls on its own,
/// because unpacking an OCI image is a human's or the CLI's job, not a boot's.
pub fn rootfs(spec: &Spec) -> Result<PathBuf> {
    let name = spec.image.as_deref().unwrap_or(DEFAULT_IMAGE);
    let path = match name.starts_with('/') {
        true => PathBuf::from(name),
        false => images_dir(spec).join(name).join(image::ROOTFS),
    };
    if !path.is_dir() {
        bail!(
            "krun needs a prepared root filesystem at {}: run `tenon sandbox image pull <ref> --name {name}`",
            path.display()
        );
    }
    Ok(path)
}

/// The one thing a host with a hypervisor can prove without a gateway, a node
/// or a harness: boot a microVM off `rootfs`, run one command in it with
/// `workspace` shared over virtio-fs, and let the host read what the guest
/// wrote. `krun_start_enter` exits the VMM process with the guest init's
/// status, so the child's exit code is the guest's.
pub fn smoke(
    binary: &Path,
    rootfs: &Path,
    workspace: &Path,
    marker: &str,
    ram_mb: u32,
) -> Result<i32> {
    let config = VmmConfig {
        rootfs: rootfs.to_path_buf(),
        workdir: GUEST_WORKSPACE.to_string(),
        exec: "/bin/sh".to_string(),
        argv: vec![
            "-c".to_string(),
            format!("echo hello-tenon > {GUEST_WORKSPACE}/{marker}"),
        ],
        env: vec![format!("PATH={GUEST_PATH}"), "HOME=/root".to_string()],
        virtiofs: vec![(GUEST_WORKSPACE.to_string(), workspace.to_path_buf())],
        ram_mb,
        vcpus: 1,
        port_map: vec![],
        vsock_ports: vec![],
        rlimits: vec![],
        console: Some(workspace.join("krun-smoke-console.log")),
        log_level: 1,
    };
    let config_file = workspace.join("krun-smoke.json");
    vmm::write(&config_file, &config)?;
    let mut child = vmm::launch(binary, &config_file, &workspace.join("krun-smoke.log"))?;
    let status = child
        .wait_timeout(SMOKE_TIMEOUT)
        .context("wait for the smoke microVM")?;
    let _ = std::fs::remove_file(&config_file);
    match status {
        Some(status) => Ok(status.code().unwrap_or(-1)),
        None => {
            proc::terminate(child, STOP_GRACE);
            bail!("the smoke microVM did not exit inside {SMOKE_TIMEOUT:?}")
        }
    }
}

impl Sandbox for Krun {
    fn backend(&self) -> &'static str {
        "krun"
    }

    fn gateway_address(&self, env: &str, _default: &str) -> Option<String> {
        Some(gateway_address(env))
    }

    fn spawn(&self, spec: &Spec) -> Result<Arc<dyn Instance>> {
        std::fs::create_dir_all(&spec.workspace)
            .with_context(|| format!("create workspace {}", spec.workspace.display()))?;
        let rootfs = rootfs(spec)?;
        let binary = PathBuf::from(crate::host_binary(spec));
        image::install_binary(&rootfs, &binary, GUEST_BINARY)?;
        let dir = spec
            .workspace
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| spec.workspace.clone());
        let passthrough = spec
            .env_passthrough
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.clone(), value)))
            .collect();
        Ok(Arc::new(KrunInstance {
            id: format!("krun:{}:{}", spec.home_hash, spec.env),
            env: spec.env.clone(),
            rootfs,
            workspace: spec.workspace.clone(),
            config_file: dir.join("krun.json"),
            log: dir.join("krun-vmm.log"),
            console: dir.join("krun-console.log"),
            binary,
            ram_mb: spec.policy.ram_mb.min(u32::MAX as u64) as u32,
            pids_max: spec.policy.pids_max,
            passthrough,
            port: gateway_port(&spec.env),
            child: Mutex::new(None),
            destroyed: AtomicBool::new(false),
        }))
    }
}

impl KrunInstance {
    /// The whole VM in one value: what the guest runs, with what environment,
    /// on what memory, sharing which host directory. Split out from
    /// `start_worker` so a test can assert the shape without a hypervisor.
    pub fn worker_config(&self, gateway: &str) -> VmmConfig {
        let mut env = vec![
            format!("TENON_GATEWAY={gateway}"),
            format!("TENON_ENV={}", self.env),
            format!("TENON_WORKSPACE={GUEST_WORKSPACE}"),
            format!("PATH={GUEST_PATH}"),
            "HOME=/root".to_string(),
        ];
        for (name, value) in &self.passthrough {
            env.push(format!("{name}={value}"));
        }
        VmmConfig {
            rootfs: self.rootfs.clone(),
            workdir: GUEST_WORKSPACE.to_string(),
            exec: GUEST_BINARY.to_string(),
            argv: vec![
                "worker".to_string(),
                "--workspace".to_string(),
                GUEST_WORKSPACE.to_string(),
            ],
            env,
            virtiofs: vec![(GUEST_WORKSPACE.to_string(), self.workspace.clone())],
            ram_mb: self.ram_mb,
            vcpus: 1,
            port_map: vec![],
            vsock_ports: vec![],
            rlimits: vec![format!("{RLIMIT_NPROC}={0}:{0}", self.pids_max)],
            console: Some(self.console.clone()),
            log_level: 1,
        }
    }
}

impl Instance for KrunInstance {
    fn id(&self) -> &str {
        &self.id
    }

    fn backend(&self) -> &'static str {
        "krun"
    }

    fn attach_addr(&self) -> Endpoint {
        Endpoint::Tcp("127.0.0.1".to_string(), self.port)
    }

    fn workspace_path(&self) -> String {
        GUEST_WORKSPACE.to_string()
    }

    fn binary_path(&self) -> String {
        GUEST_BINARY.to_string()
    }

    /// There is no exec into a running microVM: libkrun starts one process and
    /// that process is the guest. Everything base would have exec'd is either
    /// the worker (which is the init here) or a wire call to it.
    fn exec(&self, _cmd: &str, _args: &[String], _timeout: Duration) -> Result<ExecOutcome> {
        bail!("krun has no exec into a live microVM: the worker is the guest init")
    }

    fn start_worker(&self, _env: &str, gateway: &str) -> Result<bool> {
        let mut slot = self
            .child
            .lock()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if slot.is_some() {
            return Ok(true);
        }
        let config = self.worker_config(gateway);
        vmm::write(&self.config_file, &config)?;
        *slot = Some(vmm::launch(&self.binary, &self.config_file, &self.log)?);
        Ok(true)
    }

    fn destroy(&self) -> Result<()> {
        if self.destroyed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let taken = self.child.lock().ok().and_then(|mut slot| slot.take());
        if let Some(child) = taken {
            proc::terminate(child, STOP_GRACE);
        }
        let _ = std::fs::remove_file(&self.config_file);
        Ok(())
    }
}

impl Drop for KrunInstance {
    fn drop(&mut self) {
        let _ = self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Policy;

    fn spec(images: Option<PathBuf>, image: Option<String>) -> Spec {
        Spec {
            env: "root".to_string(),
            image,
            binary: Some(PathBuf::from("/usr/bin/tenon")),
            workspace: PathBuf::from("/home/x/.tenon/envs/root/workspace"),
            gateway: Some("tcp:127.0.0.1:10000".to_string()),
            env_passthrough: vec![],
            policy: Policy {
                ram_mb: 768,
                pids_max: 64,
                egress: false,
            },
            caps: vec![],
            home_hash: "abc123def456".to_string(),
            base_pid: 1,
            images,
            ingress_ports: Vec::new(),
            mounts: Vec::new(),
            hostname: None,
        }
    }

    fn instance() -> KrunInstance {
        KrunInstance {
            id: "krun:abc:root".to_string(),
            env: "root".to_string(),
            rootfs: PathBuf::from("/home/x/.tenon/images/default/rootfs"),
            workspace: PathBuf::from("/home/x/.tenon/envs/root/workspace"),
            config_file: PathBuf::from("/home/x/.tenon/envs/root/krun.json"),
            log: PathBuf::from("/home/x/.tenon/envs/root/krun-vmm.log"),
            console: PathBuf::from("/home/x/.tenon/envs/root/krun-console.log"),
            binary: PathBuf::from("/usr/bin/tenon"),
            ram_mb: 768,
            pids_max: 64,
            passthrough: vec![("TENON_DEMO".to_string(), "1".to_string())],
            port: 10007,
            child: Mutex::new(None),
            destroyed: AtomicBool::new(false),
        }
    }

    #[test]
    fn the_reason_names_the_hypervisor_and_the_library() {
        let Some(reason) = unavailable() else {
            return;
        };
        assert!(reason.starts_with("krun unavailable: "), "{reason}");
        let hypervisor = hypervisor().is_err();
        let library = ffi::load().is_err();
        assert!(hypervisor || library, "{reason}");
        if hypervisor {
            assert!(
                reason.contains("/dev/kvm") || reason.contains("HVF") || reason.contains("Linux"),
                "{reason}"
            );
        }
        if library {
            assert!(reason.contains("libkrun"), "{reason}");
        }
    }

    #[test]
    fn the_worker_is_the_guest_init_with_the_gateway_in_its_environment() {
        let config = instance().worker_config("tcp:127.0.0.1:10007");
        assert_eq!(config.exec, GUEST_BINARY);
        assert_eq!(config.argv, ["worker", "--workspace", "/workspace"]);
        assert!(config
            .env
            .contains(&"TENON_GATEWAY=tcp:127.0.0.1:10007".to_string()));
        assert!(config.env.contains(&"TENON_ENV=root".to_string()));
        assert!(config.env.contains(&"TENON_DEMO=1".to_string()));
        assert_eq!(config.workdir, GUEST_WORKSPACE);
        assert_eq!(config.ram_mb, 768);
        assert_eq!(config.rlimits, ["6=64:64"]);
        assert_eq!(
            config.virtiofs,
            [(
                GUEST_WORKSPACE.to_string(),
                PathBuf::from("/home/x/.tenon/envs/root/workspace")
            )]
        );
    }

    #[test]
    fn the_config_survives_a_round_trip_through_the_file_the_vmm_reads() {
        let config = instance().worker_config("tcp:127.0.0.1:10007");
        let dir = std::env::temp_dir().join(format!("tenon-krun-cfg-{}", std::process::id()));
        let file = dir.join("krun.json");
        vmm::write(&file, &config).unwrap();
        assert_eq!(vmm::read(&file).unwrap(), config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_gateway_port_is_stable_per_env_and_inside_the_span() {
        let root = gateway_port("root");
        assert_eq!(root, gateway_port("root"));
        assert_ne!(root, gateway_port("child"));
        assert!((DEFAULT_GATEWAY_PORT..DEFAULT_GATEWAY_PORT + GATEWAY_PORT_SPAN).contains(&root));
        assert_eq!(gateway_address("root"), format!("tcp:127.0.0.1:{root}"));
    }

    #[test]
    fn a_missing_rootfs_names_the_command_that_prepares_one() {
        let error = rootfs(&spec(Some(PathBuf::from("/nonexistent-images")), None))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("/nonexistent-images/default/rootfs"),
            "{error}"
        );
        assert!(error.contains("tenon sandbox image pull"), "{error}");
    }

    #[test]
    fn an_absolute_image_is_taken_as_the_rootfs_itself() {
        let error = rootfs(&spec(None, Some("/nonexistent-rootfs".to_string())))
            .unwrap_err()
            .to_string();
        assert!(error.contains("krun needs a prepared root filesystem at /nonexistent-rootfs"));
    }

    #[test]
    fn every_required_symbol_is_a_krun_entry_point() {
        let (api, required, optional) = api();
        assert!(api.contains("libkrun"));
        assert!(required.iter().all(|name| name.starts_with("krun_")));
        assert!(optional.iter().all(|name| name.starts_with("krun_")));
        assert!(required.contains(&"krun_start_enter"));
        assert!(required.contains(&"krun_set_exec"));
    }
}
