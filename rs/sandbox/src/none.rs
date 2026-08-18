use crate::{Endpoint, ExecOutcome, Instance, Sandbox, Spec};
use anyhow::{bail, Result};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct NoSandbox;

struct NoInstance {
    id: String,
    workspace: String,
    binary: String,
}

impl Sandbox for NoSandbox {
    fn backend(&self) -> &'static str {
        "none"
    }

    fn spawn(&self, spec: &Spec) -> Result<Arc<dyn Instance>> {
        Ok(Arc::new(NoInstance {
            id: format!("none:{}", spec.env),
            workspace: spec.workspace.display().to_string(),
            binary: crate::host_binary(spec),
        }))
    }
}

impl Instance for NoInstance {
    fn id(&self) -> &str {
        &self.id
    }

    fn backend(&self) -> &'static str {
        "none"
    }

    fn attach_addr(&self) -> Endpoint {
        Endpoint::Direct
    }

    fn workspace_path(&self) -> String {
        self.workspace.clone()
    }

    fn binary_path(&self) -> String {
        self.binary.clone()
    }

    fn exec(&self, _cmd: &str, _args: &[String], _timeout: Duration) -> Result<ExecOutcome> {
        bail!("the none backend runs no sandboxed exec")
    }

    fn destroy(&self) -> Result<()> {
        Ok(())
    }
}
