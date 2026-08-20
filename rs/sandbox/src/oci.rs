use crate::{proc, Endpoint, ExecOutcome, Instance, Sandbox, Spec};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_IMAGE: &str = "python:3.12-slim";
const GUEST_WORKSPACE: &str = "/workspace";
const GUEST_BINARY: &str = "/usr/local/bin/tenon";
const ENV_LABEL: &str = "tenon.env";
const HOME_LABEL: &str = "tenon.home";
const BASE_LABEL: &str = "tenon.base";
const STOP_GRACE_SECS: &str = "2";
const EXEC_GRACE: Duration = Duration::from_secs(3);

pub struct Oci {
    cli: &'static str,
}

pub struct OciInstance {
    id: String,
    cli: &'static str,
    destroyed: AtomicBool,
    gateway: Option<String>,
    /// container ingress port -> the host `127.0.0.1:<port>` it is published on.
    ingress: HashMap<u16, String>,
}

pub fn probe() -> Result<Box<dyn Sandbox>, String> {
    match find_cli() {
        Some(cli) => Ok(Box::new(Oci { cli })),
        None => Err("neither podman nor docker found in PATH".to_string()),
    }
}

fn find_cli() -> Option<&'static str> {
    if in_path("podman") {
        Some("podman")
    } else if in_path("docker") {
        Some("docker")
    } else {
        None
    }
}

fn in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

impl Sandbox for Oci {
    fn backend(&self) -> &'static str {
        "oci"
    }

    fn spawn(&self, spec: &Spec) -> Result<Arc<dyn Instance>> {
        std::fs::create_dir_all(&spec.workspace)
            .with_context(|| format!("create workspace {}", spec.workspace.display()))?;
        let image = spec.image.as_deref().unwrap_or(DEFAULT_IMAGE);
        let name = container_name(&spec.env, &spec.home_hash);
        let mut args: Vec<String> = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            name.clone(),
            "--label".to_string(),
            format!("{ENV_LABEL}={}", spec.env),
            "--label".to_string(),
            format!("{HOME_LABEL}={}", spec.home_hash),
            "--label".to_string(),
            format!("{BASE_LABEL}={}", spec.base_pid),
            "--memory".to_string(),
            format!("{}m", spec.policy.ram_mb),
            "--pids-limit".to_string(),
            spec.policy.pids_max.to_string(),
            "-v".to_string(),
            format!("{}:{GUEST_WORKSPACE}", spec.workspace.display()),
        ];
        let binary = crate::host_binary(spec);
        if std::path::Path::new(&binary).is_file() {
            args.push("-v".to_string());
            args.push(format!("{binary}:{GUEST_BINARY}:ro"));
        }
        if let Some(address) = &spec.gateway {
            if let Some(dir) = crate::gateway_dir(address) {
                args.push("-v".to_string());
                args.push(format!("{}:{}:rw", dir.display(), dir.display()));
            }
            if address.starts_with("tcp:") {
                args.push("--network".to_string());
                args.push("host".to_string());
            }
            args.push("-e".to_string());
            args.push(format!("TENON_GATEWAY={address}"));
        }
        for name in &spec.env_passthrough {
            if let Ok(value) = std::env::var(name) {
                args.push("-e".to_string());
                args.push(format!("{name}={value}"));
            }
        }
        // RFC 8c ingress (P4.5): publish each container-side app port on a free
        // 127.0.0.1 host port. podman 4.x refuses host port 0, so a free port is
        // chosen on the host first; the small window until the engine binds it is
        // the same race every ephemeral-port scheme has.
        let mut ingress: HashMap<u16, String> = HashMap::new();
        for cport in &spec.ingress_ports {
            let host_port = free_host_port()?;
            args.push("-p".to_string());
            args.push(format!("127.0.0.1:{host_port}:{cport}"));
            ingress.insert(*cport, format!("127.0.0.1:{host_port}"));
        }
        if !spec.ingress_ports.is_empty() {
            let csv: Vec<String> = spec.ingress_ports.iter().map(u16::to_string).collect();
            args.push("-e".to_string());
            args.push(format!("TENON_INGRESS_PORTS={}", csv.join(",")));
        }
        args.push(image.to_string());
        args.push("sleep".to_string());
        args.push("infinity".to_string());

        let mut command = Command::new(self.cli);
        command.args(&args);
        let outcome = proc::run(command, Duration::from_secs(180))?;
        if outcome.status != 0 {
            bail!(
                "{} run failed: {}",
                self.cli,
                String::from_utf8_lossy(&outcome.stderr)
            );
        }
        Ok(Arc::new(OciInstance {
            id: name,
            cli: self.cli,
            destroyed: AtomicBool::new(false),
            gateway: spec.gateway.clone(),
            ingress,
        }))
    }

    fn reap(&self, home_hash: &str, all: bool) -> Result<usize> {
        let mut command = Command::new(self.cli);
        command.args([
            "ps",
            "-a",
            "--filter",
            &format!("label={HOME_LABEL}={home_hash}"),
            "--format",
            "{{.ID}}",
        ]);
        let outcome = proc::run(command, Duration::from_secs(15))?;
        let mut reaped = 0usize;
        for id in String::from_utf8_lossy(&outcome.stdout).lines() {
            let id = id.trim();
            if id.is_empty() {
                continue;
            }
            if (all || !self.base_alive(id)) && self.run_orphan(id).is_ok() {
                reaped += 1;
            }
        }
        Ok(reaped)
    }
}

impl Oci {
    fn run_orphan(&self, id: &str) -> Result<ExecOutcome> {
        let mut command = Command::new(self.cli);
        command.args(["rm", "-f", id]);
        proc::run(command, Duration::from_secs(15))
    }

    /// True unless the container names a `tenon.base` pid we can positively prove
    /// is gone. Any failure to read the label, or a label that does not parse,
    /// counts as alive so a reap pass never removes something it cannot judge.
    fn base_alive(&self, id: &str) -> bool {
        let mut command = Command::new(self.cli);
        command.args([
            "inspect",
            id,
            "--format",
            &format!("{{{{ index .Config.Labels \"{BASE_LABEL}\" }}}}"),
        ]);
        let Ok(outcome) = proc::run(command, Duration::from_secs(10)) else {
            return true;
        };
        if outcome.status != 0 {
            return true;
        }
        let text = String::from_utf8_lossy(&outcome.stdout);
        match text.trim().parse::<i32>() {
            Ok(pid) => proc::alive(pid),
            Err(_) => true,
        }
    }
}

fn free_host_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .context("reserve a free host port for ingress")?;
    Ok(listener.local_addr()?.port())
}

fn container_name(env: &str, home_hash: &str) -> String {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let clean: String = env
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    format!("tenon-{home_hash}-{clean}-{suffix}")
}

impl OciInstance {
    fn run(&self, args: &[&str]) -> Result<ExecOutcome> {
        let mut command = Command::new(self.cli);
        command.args(args);
        proc::run(command, Duration::from_secs(30))
    }
}

impl Instance for OciInstance {
    fn id(&self) -> &str {
        &self.id
    }

    fn backend(&self) -> &'static str {
        "oci"
    }

    fn workspace_path(&self) -> String {
        GUEST_WORKSPACE.to_string()
    }

    fn binary_path(&self) -> String {
        GUEST_BINARY.to_string()
    }

    fn attach_addr(&self) -> Endpoint {
        match &self.gateway {
            Some(address) if address.starts_with("unix:") => {
                Endpoint::Uds(PathBuf::from(&address[5..]))
            }
            Some(address) if address.starts_with("tcp:") => {
                let rest = &address[4..];
                match rest.rsplit_once(':') {
                    Some((host, port)) => {
                        Endpoint::Tcp(host.to_string(), port.parse().unwrap_or(0))
                    }
                    None => Endpoint::Direct,
                }
            }
            _ => Endpoint::Direct,
        }
    }

    fn exec(&self, cmd: &str, args: &[String], timeout: Duration) -> Result<ExecOutcome> {
        let secs = timeout.as_secs().max(1).to_string();
        let mut command = Command::new(self.cli);
        command.arg("exec").arg(&self.id);
        command.arg("timeout").arg("-s").arg("KILL").arg(&secs);
        command.arg(cmd).args(args);
        let mut outcome = proc::run(command, timeout + EXEC_GRACE)?;
        if outcome.status == 137 {
            outcome.timed_out = true;
        }
        Ok(outcome)
    }

    fn ingress_addr(&self, container_port: u16) -> Option<String> {
        self.ingress.get(&container_port).cloned()
    }

    fn destroy(&self) -> Result<()> {
        if self.destroyed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Graceful stop first (grace, then the engine's own SIGKILL); `rm -f` after
        // is the unconditional kill-and-remove fallback regardless of what state
        // stop left the container in, so a container that is already gone (never
        // started, raced with another reaper) is not treated as a failure.
        let _ = self.run(&["stop", "-t", STOP_GRACE_SECS, &self.id]);
        let Ok(outcome) = self.run(&["rm", "-f", &self.id]) else {
            return Ok(());
        };
        if outcome.status != 0 {
            let text = String::from_utf8_lossy(&outcome.stderr).to_lowercase();
            if !text.contains("no such") && !text.contains("does not exist") {
                bail!("{} rm failed: {text}", self.cli);
            }
        }
        Ok(())
    }
}

impl Drop for OciInstance {
    fn drop(&mut self) {
        let _ = self.destroy();
    }
}
