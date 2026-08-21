use std::time::Duration;
use tenon_sandbox::Instance;

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

/// A zero-cost auth preflight (RFC P5.0-v2, deliverable 2). `probes` are
/// read-only commands run INSIDE THE SANDBOX (e.g. `agy --version`,
/// `agy models`) that spend nothing but exercise the same credential paths a
/// real run needs. If the container blocks a cred path, the agent prints an
/// auth-failure signature and the preflight fails loud without a model call.
pub struct PreflightSpec {
    pub cmd: String,
    pub probes: Vec<Vec<String>>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    pub timeout: Duration,
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

/// Run each probe inside `instance` in turn; the first that prints an
/// auth-failure signature or exits non-zero fails the preflight. A clean pass
/// across every probe is the only thing that clears a paid run. The probe env
/// carries the same credential/home wiring the real run uses, so a blocked
/// cred volume fails here and never on a paid call.
pub fn run(instance: &dyn Instance, spec: &PreflightSpec) -> anyhow::Result<Preflight> {
    for probe in &spec.probes {
        let argv = with_cwd(&spec.cwd, &spec.cmd, probe, &spec.env);
        let outcome = instance.exec("sh", &argv, spec.timeout)?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        if let Some(signature) = auth_failure_signature(&combined) {
            return Ok(Preflight {
                ok: false,
                probe: probe.join(" "),
                signature: Some(signature),
                detail: tail(&combined),
            });
        }
        if outcome.timed_out {
            return Ok(Preflight {
                ok: false,
                probe: probe.join(" "),
                signature: None,
                detail: format!("timed out: {}", tail(&combined)),
            });
        }
        if outcome.status != 0 {
            return Ok(Preflight {
                ok: false,
                probe: probe.join(" "),
                signature: None,
                detail: format!("exit {}: {}", outcome.status, tail(&combined)),
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

/// Build a `sh -c` line that cds into the agent cwd, exports the run's env and
/// runs `cmd probe...`. One `sh -c` keeps the cwd and env identical to the real
/// streamed run without a second exec API on the backend.
fn with_cwd(cwd: &str, cmd: &str, probe: &[String], env: &[(String, String)]) -> Vec<String> {
    let mut script = format!("cd {} 2>/dev/null; ", shell_quote(cwd));
    for (key, value) in env {
        script.push_str(&format!("export {}={}; ", key, shell_quote(value)));
    }
    script.push_str(&shell_quote(cmd));
    for arg in probe {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    vec!["-c".to_string(), script]
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

    #[test]
    fn with_cwd_cds_and_exports() {
        let argv = with_cwd(
            "/workspace",
            "agy",
            &["models".to_string()],
            &[("HOME".to_string(), "/root".to_string())],
        );
        assert_eq!(argv[0], "-c");
        assert!(argv[1].contains("cd '/workspace'"));
        assert!(argv[1].contains("export HOME='/root'"));
        assert!(argv[1].trim_end().ends_with("'agy' 'models'"));
    }
}
