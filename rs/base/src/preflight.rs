use crate::jail::{self, JailSpec, Limits};
use std::io::Read;
use std::path::PathBuf;

/// Case-insensitive substrings that mean the agent could not authenticate. A
/// blocked credential path shows up as one of these on an otherwise zero-cost
/// command, so the preflight refuses the paid run before a single model call.
const SIGNATURES: &[&str] = &[
    "license",
    "not logged in",
    "unauthenticated",
    "login",
    "expired",
    "forbidden",
    "401",
];

/// A zero-cost auth preflight (RFC P5.0, deliverable 2). `probes` are read-only
/// commands run UNDER THE JAIL (e.g. `agy --version`, `agy mcp list`) that spend
/// nothing but exercise the same credential paths a real run needs. If the jail
/// blocks a cred path, the agent prints an auth-failure signature and the
/// preflight fails loud without proceeding to the model.
pub struct PreflightSpec {
    pub cmd: String,
    pub probes: Vec<Vec<String>>,
    pub scratch: PathBuf,
    pub tmp: PathBuf,
    pub agent_home: PathBuf,
    pub rw_allow: Vec<PathBuf>,
    pub ro_allow: Vec<PathBuf>,
    pub limits: Limits,
    pub env: Vec<(String, String)>,
}

pub struct Preflight {
    pub ok: bool,
    pub probe: String,
    pub signature: Option<String>,
    pub detail: String,
}

/// The single scan the preflight and its tests share: the first auth-failure
/// signature in `text`, case-insensitive, or `None` for clean output.
pub fn auth_failure_signature(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    SIGNATURES
        .iter()
        .find(|needle| lower.contains(**needle))
        .map(|needle| (*needle).to_string())
}

/// Run each probe under the jail in turn; the first that prints an auth-failure
/// signature or exits non-zero fails the preflight. A clean pass across every
/// probe is the only thing that clears a paid run.
pub fn run(spec: &PreflightSpec) -> anyhow::Result<Preflight> {
    std::fs::create_dir_all(&spec.scratch)?;
    std::fs::create_dir_all(&spec.tmp)?;
    for probe in &spec.probes {
        let jail_spec = JailSpec {
            cmd: spec.cmd.clone(),
            args: probe.clone(),
            cwd: spec.scratch.clone(),
            scratch: spec.scratch.clone(),
            tmp: spec.tmp.clone(),
            rw_allow: spec.rw_allow.clone(),
            ro_allow: spec.ro_allow.clone(),
            env: spec.env.clone(),
            limits: spec.limits.clone(),
            cgroup_parent: None,
        };
        let mut jail = jail::spawn(&jail_spec)?;
        let mut out = String::new();
        if let Some(mut stdout) = jail.child.stdout.take() {
            let _ = stdout.read_to_string(&mut out);
        }
        let mut err = String::new();
        if let Some(mut stderr) = jail.child.stderr.take() {
            let _ = stderr.read_to_string(&mut err);
        }
        let status = jail
            .child
            .wait()
            .map(|status| status.code().unwrap_or(-1))
            .unwrap_or(-1);
        jail.kill();
        let combined = format!("{out}\n{err}");
        if let Some(signature) = auth_failure_signature(&combined) {
            return Ok(Preflight {
                ok: false,
                probe: probe.join(" "),
                signature: Some(signature),
                detail: tail(&combined),
            });
        }
        if status != 0 {
            return Ok(Preflight {
                ok: false,
                probe: probe.join(" "),
                signature: None,
                detail: format!("exit {status}: {}", tail(&combined)),
            });
        }
    }
    Ok(Preflight {
        ok: true,
        probe: String::new(),
        signature: None,
        detail: "clean".to_string(),
    })
}

fn tail(text: &str) -> String {
    let trimmed = text.trim();
    let start = trimmed.len().saturating_sub(400);
    trimmed[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_case_insensitive() {
        assert_eq!(
            auth_failure_signature("Error: LICENSE not valid").as_deref(),
            Some("license")
        );
        assert_eq!(
            auth_failure_signature("please login to continue").as_deref(),
            Some("login")
        );
        assert_eq!(auth_failure_signature("agy version 1.1.17"), None);
        assert_eq!(auth_failure_signature("No MCP servers configured."), None);
    }
}
