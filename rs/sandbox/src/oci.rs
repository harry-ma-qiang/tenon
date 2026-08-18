use crate::{proc, Endpoint, ExecOutcome, Instance, Sandbox, Spec};
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_IMAGE: &str = "python:3.12-alpine";
const LABEL: &str = "tenon.env";
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
        let name = container_name(&spec.env);
        let mut args: Vec<String> = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            name.clone(),
            "--label".to_string(),
            format!("{LABEL}={}", spec.env),
            "--memory".to_string(),
            format!("{}m", spec.policy.ram_mb),
            "--pids-limit".to_string(),
            spec.policy.pids_max.to_string(),
            "-v".to_string(),
            format!("{}:/workspace", spec.workspace.display()),
        ];
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
        }))
    }

    fn reap(&self, env: &str) -> Result<()> {
        let mut command = Command::new(self.cli);
        command.args([
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL}={env}"),
            "--format",
            "{{.ID}}",
        ]);
        let outcome = proc::run(command, Duration::from_secs(15))?;
        for id in String::from_utf8_lossy(&outcome.stdout).lines() {
            let id = id.trim();
            if !id.is_empty() {
                let _ = self.run_orphan(id);
            }
        }
        Ok(())
    }
}

impl Oci {
    fn run_orphan(&self, id: &str) -> Result<ExecOutcome> {
        let mut command = Command::new(self.cli);
        command.args(["rm", "-f", id]);
        proc::run(command, Duration::from_secs(15))
    }
}

fn container_name(env: &str) -> String {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let clean: String = env
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    format!("tenon-{clean}-{suffix}")
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

    fn destroy(&self) -> Result<()> {
        if self.destroyed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.run(&["stop", "-t", STOP_GRACE_SECS, &self.id]);
        let outcome = self.run(&["rm", "-f", &self.id])?;
        if outcome.status != 0 {
            let text = String::from_utf8_lossy(&outcome.stderr);
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
