use super::ffi;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::ffi::{c_char, CString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Everything the VMM child needs, written to a file base owns and handed over
/// by path. It is a file rather than argv because the guest environment is in
/// it, and because a config that grows never runs into an argv limit.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub rootfs: PathBuf,
    pub workdir: String,
    pub exec: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    /// `(guest mount point, host directory)`; the mount point doubles as the
    /// virtio-fs tag, which is how libkrun's init knows where to mount it.
    pub virtiofs: Vec<(String, PathBuf)>,
    pub ram_mb: u32,
    pub vcpus: u8,
    /// `host_port:guest_port` rows for `krun_set_port_map` (TSI inbound).
    pub port_map: Vec<String>,
    /// `(guest vsock port, host unix socket path)` rows for
    /// `krun_add_vsock_port`: the bridge used when the gateway cannot listen on
    /// TCP. Empty in the default shape.
    pub vsock_ports: Vec<(u32, PathBuf)>,
    pub rlimits: Vec<String>,
    pub console: Option<PathBuf>,
    pub log_level: u32,
}

pub fn write(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(config).context("encode the krun config")?;
    std::fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn read(path: &Path) -> Result<Config> {
    let body = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))
}

/// Starts the VMM in its own process. libkrun takes over the calling process
/// (`krun_start_enter` calls `exit()` when the guest shuts down) and a
/// `fork()` out of a threaded tokio runtime may only call async-signal-safe
/// functions, so base re-execs its own binary instead: one fresh,
/// single-threaded process per microVM, whose pid is the VM's lifetime.
pub fn launch(binary: &Path, config_file: &Path, log: &Path) -> Result<Child> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .with_context(|| format!("open {}", log.display()))?;
    let child = Command::new(binary)
        .arg("sandbox")
        .arg("vmm")
        .arg("--config")
        .arg(config_file)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone().context("clone the vmm log")?))
        .stderr(Stdio::from(log))
        .spawn()
        .with_context(|| format!("spawn {} sandbox vmm", binary.display()))?;
    Ok(child)
}

fn cstring(text: &str) -> Result<CString> {
    CString::new(text).with_context(|| format!("{text:?} has an interior NUL"))
}

struct Argv {
    _owned: Vec<CString>,
    pointers: Vec<*const c_char>,
}

impl Argv {
    fn new(items: &[String]) -> Result<Self> {
        let owned = items
            .iter()
            .map(|item| cstring(item))
            .collect::<Result<Vec<_>>>()?;
        let mut pointers: Vec<*const c_char> = owned.iter().map(|item| item.as_ptr()).collect();
        pointers.push(std::ptr::null());
        Ok(Self {
            _owned: owned,
            pointers,
        })
    }

    fn as_ptr(&self) -> *const *const c_char {
        self.pointers.as_ptr()
    }
}

fn check(name: &str, code: i32) -> Result<()> {
    if code < 0 {
        bail!(
            "{name} failed: {} ({code})",
            std::io::Error::from_raw_os_error(-code)
        );
    }
    Ok(())
}

/// The body of `tenon sandbox vmm`: configure one microVM and enter it. Only
/// returns on a configuration error — on success libkrun exits this process
/// with the guest init's status.
pub fn main(config: &Config) -> Result<i32> {
    let api = ffi::load().map_err(|reason| anyhow::anyhow!("{reason}"))?;
    unsafe {
        check("krun_set_log_level", (api.set_log_level)(config.log_level))?;
        let ctx = (api.create_ctx)();
        check("krun_create_ctx", ctx)?;
        let ctx = ctx as u32;
        check(
            "krun_set_vm_config",
            (api.set_vm_config)(ctx, config.vcpus.max(1), config.ram_mb.max(64)),
        )?;
        let root = cstring(&config.rootfs.display().to_string())?;
        check("krun_set_root", (api.set_root)(ctx, root.as_ptr()))?;
        for (tag, host) in &config.virtiofs {
            let tag = cstring(tag)?;
            let host = cstring(&host.display().to_string())?;
            check(
                "krun_add_virtiofs",
                (api.add_virtiofs)(ctx, tag.as_ptr(), host.as_ptr()),
            )?;
        }
        if !config.port_map.is_empty() {
            let Some(set_port_map) = api.set_port_map else {
                bail!("this libkrun has no krun_set_port_map: it is built without TSI");
            };
            let map = Argv::new(&config.port_map)?;
            check("krun_set_port_map", set_port_map(ctx, map.as_ptr()))?;
        }
        for (port, path) in &config.vsock_ports {
            let Some(add_vsock_port) = api.add_vsock_port else {
                bail!("this libkrun has no krun_add_vsock_port");
            };
            let path = cstring(&path.display().to_string())?;
            check(
                "krun_add_vsock_port",
                add_vsock_port(ctx, *port, path.as_ptr()),
            )?;
        }
        if !config.rlimits.is_empty() {
            if let Some(set_rlimits) = api.set_rlimits {
                let limits = Argv::new(&config.rlimits)?;
                check("krun_set_rlimits", set_rlimits(ctx, limits.as_ptr()))?;
            }
        }
        if let (Some(set_console_output), Some(path)) = (api.set_console_output, &config.console) {
            let path = cstring(&path.display().to_string())?;
            check(
                "krun_set_console_output",
                set_console_output(ctx, path.as_ptr()),
            )?;
        }
        let workdir = cstring(&config.workdir)?;
        check("krun_set_workdir", (api.set_workdir)(ctx, workdir.as_ptr()))?;
        let env = Argv::new(&config.env)?;
        check("krun_set_env", (api.set_env)(ctx, env.as_ptr()))?;
        let exec = cstring(&config.exec)?;
        let argv = Argv::new(&config.argv)?;
        // envp is NULL on purpose: krun_set_env above is the environment, and
        // passing both would leave libkrun to merge two lists.
        check(
            "krun_set_exec",
            (api.set_exec)(ctx, exec.as_ptr(), argv.as_ptr(), std::ptr::null()),
        )?;
        let code = (api.start_enter)(ctx);
        if let Some(free_ctx) = api.free_ctx {
            let _ = free_ctx(ctx);
        }
        check("krun_start_enter", code)?;
        Ok(code)
    }
}
