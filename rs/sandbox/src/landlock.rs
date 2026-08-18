use crate::{proc, Endpoint, ExecOutcome, Instance, Sandbox, Spec};
use anyhow::{Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, RestrictionStatus, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetError, ABI,
};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

const RO_PATHS: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/bin",
    "/sbin",
    "/etc",
    "/proc/self",
];

pub struct Landlock;

pub struct LandlockInstance {
    id: String,
    workspace: PathBuf,
    gateway_dir: Option<PathBuf>,
    binary: String,
}

pub fn probe() -> Result<Box<dyn Sandbox>, String> {
    match Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|ruleset| ruleset.create())
    {
        Ok(_) => Ok(Box::new(Landlock)),
        Err(error) => Err(format!("landlock unavailable: {error}")),
    }
}

impl Sandbox for Landlock {
    fn backend(&self) -> &'static str {
        "landlock"
    }

    fn spawn(&self, spec: &Spec) -> Result<Arc<dyn Instance>> {
        std::fs::create_dir_all(&spec.workspace)
            .with_context(|| format!("create workspace {}", spec.workspace.display()))?;
        let gateway_dir = spec.gateway.as_deref().and_then(crate::gateway_dir);
        Ok(Arc::new(LandlockInstance {
            id: format!("landlock:{}", spec.env),
            workspace: spec.workspace.clone(),
            gateway_dir,
            binary: crate::host_binary(spec),
        }))
    }
}

fn restrict(
    workspace: &Path,
    gateway_dir: Option<&Path>,
) -> Result<RestrictionStatus, RulesetError> {
    let abi = ABI::V2;
    let mut rw_paths: Vec<PathBuf> = vec![workspace.to_path_buf()];
    if let Some(dir) = gateway_dir {
        rw_paths.push(dir.to_path_buf());
    }
    Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?
        .add_rules(path_beneath_rules(RO_PATHS, AccessFs::from_read(abi)))?
        .add_rules(path_beneath_rules(&rw_paths, AccessFs::from_all(abi)))?
        .restrict_self()
}

impl Instance for LandlockInstance {
    fn id(&self) -> &str {
        &self.id
    }

    fn backend(&self) -> &'static str {
        "landlock"
    }

    fn attach_addr(&self) -> Endpoint {
        Endpoint::Direct
    }

    fn workspace_path(&self) -> String {
        self.workspace.display().to_string()
    }

    fn binary_path(&self) -> String {
        self.binary.clone()
    }

    fn exec(&self, cmd: &str, args: &[String], timeout: Duration) -> Result<ExecOutcome> {
        let workspace = self.workspace.clone();
        let gateway_dir = self.gateway_dir.clone();
        let mut command = Command::new(cmd);
        command.args(args).current_dir(&self.workspace);
        unsafe {
            command.pre_exec(move || {
                restrict(&workspace, gateway_dir.as_deref())
                    .map(|_status| ())
                    .map_err(|error| std::io::Error::other(error.to_string()))
            });
        }
        proc::run(command, timeout)
    }

    fn destroy(&self) -> Result<()> {
        Ok(())
    }
}
