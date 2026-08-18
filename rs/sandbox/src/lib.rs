use anyhow::{bail, Result};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
pub struct Policy {
    pub ram_mb: u64,
    pub egress: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            ram_mb: 512,
            egress: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Spec {
    pub env: String,
    pub image: Option<String>,
    pub workspace: PathBuf,
    pub policy: Policy,
    pub caps: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Endpoint {
    Direct,
    Uds(PathBuf),
    Vsock(u32),
}

#[derive(Debug, Clone, Serialize)]
pub struct Instance {
    pub id: String,
    pub backend: &'static str,
    pub endpoint: Endpoint,
}

pub trait Sandbox: Send + Sync {
    fn backend(&self) -> &'static str;
    fn spawn(&self, spec: &Spec) -> Result<Instance>;
    fn destroy(&self, instance: &Instance) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn backend(&self) -> &'static str {
        "none"
    }

    fn spawn(&self, spec: &Spec) -> Result<Instance> {
        Ok(Instance {
            id: format!("none:{}", spec.env),
            backend: self.backend(),
            endpoint: Endpoint::Direct,
        })
    }

    fn destroy(&self, _instance: &Instance) -> Result<()> {
        Ok(())
    }
}

pub fn backend(name: &str) -> Result<Box<dyn Sandbox>> {
    match name {
        "none" => Ok(Box::new(NoSandbox)),
        "oci" | "landlock" | "krun" => bail!("sandbox backend {name} arrives in P3.1"),
        other => bail!("unknown sandbox backend {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(name: &str) -> String {
        match backend(name) {
            Ok(_backend) => String::new(),
            Err(error) => error.to_string(),
        }
    }

    fn spec() -> Spec {
        Spec {
            env: "root".to_string(),
            image: None,
            workspace: PathBuf::from("/tmp/tenon-workspace"),
            policy: Policy::default(),
            caps: vec![],
        }
    }

    #[test]
    fn the_none_backend_hands_back_a_direct_instance() {
        let sandbox = backend("none").unwrap();
        let instance = sandbox.spawn(&spec()).unwrap();
        assert_eq!(instance.backend, "none");
        assert_eq!(instance.endpoint, Endpoint::Direct);
        assert_eq!(instance.id, "none:root");
        sandbox.destroy(&instance).unwrap();
    }

    #[test]
    fn the_later_backends_name_their_phase() {
        assert!(message("oci").contains("P3.1"));
        assert!(message("krun").contains("P3.1"));
        assert!(message("qemu").contains("unknown"));
    }
}
