//! `tenon doctor`: one-shot, human-triggered install and environment
//! diagnostics. It never needs a running base — it opens the home, probes the
//! toolchain and sandbox backends, checks whether the serve port is free, and
//! runs `integrity_check` over the state files it finds. It is the offline half
//! of the shared probe catalog: the guardian owns the runtime health probes
//! (`base/env/tree/worker/harness/budgets/violations`, see
//! `beam/lib/tenon/beam/guardian/probes.ex`) and drives auto-reset with them;
//! doctor owns these install-only probes and runs once. Each probe is a name
//! plus a pure check that returns ok/warn/fail and a human-readable detail.

use crate::config::Config;
use crate::home::Home;
use crate::integrity;
use anyhow::Result;
use serde_json::json;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

const SERVE_PORT: &str = "127.0.0.1:8791";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

struct Report {
    name: &'static str,
    level: Level,
    detail: String,
}

fn ok(name: &'static str, detail: impl Into<String>) -> Report {
    Report {
        name,
        level: Level::Ok,
        detail: detail.into(),
    }
}

fn warn(name: &'static str, detail: impl Into<String>) -> Report {
    Report {
        name,
        level: Level::Warn,
        detail: detail.into(),
    }
}

fn fail(name: &'static str, detail: impl Into<String>) -> Report {
    Report {
        name,
        level: Level::Fail,
        detail: detail.into(),
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The release doctor can find without booting: an explicit `--release-dir`
/// (via `TENON_RELEASE_DIR`) or an already-extracted `erts/<tag>/bin/tenon_beam`
/// under the home. The embedded payload is invisible from here — it lives in the
/// cli crate and extracts on first `start` — so a home that has only ever been
/// scaffolded warns rather than fails.
fn release_probe(home: &Home) -> Report {
    let version = env!("CARGO_PKG_VERSION");
    if let Some(dir) = std::env::var_os("TENON_RELEASE_DIR") {
        let dir = PathBuf::from(dir);
        return match dir.join("bin/tenon_beam").is_file() {
            true => ok("release", format!("v{version}, erts at {}", dir.display())),
            false => fail(
                "release",
                format!(
                    "TENON_RELEASE_DIR {} holds no bin/tenon_beam",
                    dir.display()
                ),
            ),
        };
    }
    match extracted_release(&home.erts()) {
        Some(dir) => ok("release", format!("v{version}, erts at {}", dir.display())),
        None => warn(
            "release",
            format!(
                "v{version}, no extracted erts yet (the embedded payload unpacks on first start)"
            ),
        ),
    }
}

fn extracted_release(erts: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(erts).ok()?;
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.join("bin/tenon_beam").is_file() {
            return Some(dir);
        }
    }
    None
}

fn python_probe() -> Report {
    match which("python3") {
        Some(path) => ok("python3", path.display().to_string()),
        None => warn(
            "python3",
            "not on PATH (needed by python plugins and MCP servers)",
        ),
    }
}

fn container_probe() -> Report {
    for engine in ["podman", "docker"] {
        if let Some(path) = which(engine) {
            return ok("container", format!("{engine} at {}", path.display()));
        }
    }
    warn(
        "container",
        "no podman or docker on PATH (the oci sandbox backend is unavailable)",
    )
}

/// The sandbox backends `detect()` sees, in one line: which one `auto` would
/// pick and why every earlier one was skipped — the krun reason on this kind of
/// box is exactly what a human wants doctor to surface.
fn sandbox_probe() -> Report {
    let detected = tenon_sandbox::detect();
    let chosen = detected.sandbox.backend();
    let skipped: Vec<String> = detected
        .skipped
        .iter()
        .filter(|skip| skip.backend != chosen)
        .map(|skip| format!("{}: {}", skip.backend, skip.reason))
        .collect();
    let detail = match skipped.is_empty() {
        true => format!("using {chosen}"),
        false => format!("using {chosen}; skipped {}", skipped.join("; ")),
    };
    match chosen {
        "none" => warn("sandbox", detail),
        _ => ok("sandbox", detail),
    }
}

/// Whether the default serve port is bindable. A serve already holding it is not
/// an error — it is a warning, since serve's port is a `--http` argument a human
/// may have moved.
fn serve_port_probe() -> Report {
    match TcpListener::bind(SERVE_PORT) {
        Ok(_) => ok("serve_port", format!("{SERVE_PORT} is free")),
        Err(error) => warn(
            "serve_port",
            format!("{SERVE_PORT} is not bindable ({error}); a serve may already hold it"),
        ),
    }
}

/// The barebone `state.sqlite` and every `state-<env>.sqlite`, each run through
/// the same `integrity_check` base runs at boot. A missing file is not a fault —
/// a fresh home has none yet.
fn integrity_probe(home: &Home) -> Report {
    let mut checked = 0usize;
    let mut corrupt = Vec::new();
    for path in state_files(home) {
        if !path.is_file() {
            continue;
        }
        checked += 1;
        if !integrity::is_healthy(&path) {
            corrupt.push(path.display().to_string());
        }
    }
    if !corrupt.is_empty() {
        return fail(
            "state_integrity",
            format!("corrupt: {}", corrupt.join(", ")),
        );
    }
    match checked {
        0 => ok("state_integrity", "no state files yet"),
        n => ok(
            "state_integrity",
            format!("{n} state file(s) pass integrity_check"),
        ),
    }
}

fn state_files(home: &Home) -> Vec<PathBuf> {
    let mut files = vec![home.state_file()];
    if let Ok(entries) = std::fs::read_dir(&home.root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("state-") && name.ends_with(".sqlite") {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    files
}

/// Parses `config.yml` without writing one: `Config::load` scaffolds a default
/// when the file is absent, which doctor must not do to a home it only inspects.
fn config_probe(home: &Home) -> Report {
    let path = home.config_file();
    if !path.is_file() {
        return ok("config", "no config.yml (built-in defaults apply)");
    }
    match std::fs::read_to_string(&path) {
        Err(error) => fail("config", format!("unreadable: {error}")),
        Ok(body) => match serde_yaml::from_str::<Config>(&body) {
            Ok(config) => ok("config", format!("valid, sandbox={}", config.sandbox)),
            Err(error) => fail("config", format!("invalid: {error}")),
        },
    }
}

fn reports(home: &Home) -> Vec<Report> {
    vec![
        release_probe(home),
        python_probe(),
        container_probe(),
        sandbox_probe(),
        serve_port_probe(),
        integrity_probe(home),
        config_probe(home),
    ]
}

/// `tenon doctor [--home DIR]`: run every install probe once, print a report,
/// and exit non-zero if any probe failed.
pub fn run(home: Option<PathBuf>) -> Result<i32> {
    let home = Home::resolve(home)?;
    let reports = reports(&home);
    println!("tenon doctor: {}", home.root.display());
    let mut failed = 0;
    let mut warned = 0;
    for report in &reports {
        println!(
            "  [{}] {:<15} {}",
            report.level.tag(),
            report.name,
            report.detail
        );
        match report.level {
            Level::Fail => failed += 1,
            Level::Warn => warned += 1,
            Level::Ok => {}
        }
    }
    let summary = json!({
        "home": home.root,
        "ok": reports.len() - failed - warned,
        "warn": warned,
        "fail": failed,
    });
    println!("tenon doctor: {summary}");
    Ok(match failed {
        0 => 0,
        _ => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(label: &str) -> Home {
        let root =
            std::env::temp_dir().join(format!("tenon-doctor-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = Home { root };
        home.scaffold().expect("scaffold");
        home
    }

    #[test]
    fn a_scaffolded_home_has_no_failures() {
        let home = temp_home("clean");
        let reports = reports(&home);
        assert!(reports.iter().any(|r| r.name == "sandbox"));
        assert!(
            reports.iter().all(|r| r.level != Level::Fail),
            "unexpected failure in a clean home: {:?}",
            reports
                .iter()
                .filter(|r| r.level == Level::Fail)
                .map(|r| (r.name, r.detail.clone()))
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&home.root);
    }

    #[test]
    fn a_corrupt_state_file_fails_the_integrity_probe() {
        let home = temp_home("corrupt");
        std::fs::write(home.state_file(), b"this is not a sqlite file").expect("write");
        let report = integrity_probe(&home);
        assert_eq!(report.level, Level::Fail, "{}", report.detail);
        assert!(report.detail.contains("corrupt"), "{}", report.detail);
        let _ = std::fs::remove_dir_all(&home.root);
    }

    #[test]
    fn missing_config_is_ok_and_writes_nothing() {
        let home = temp_home("noconfig");
        let report = config_probe(&home);
        assert_eq!(report.level, Level::Ok);
        assert!(
            !home.config_file().is_file(),
            "doctor must not scaffold config"
        );
        let _ = std::fs::remove_dir_all(&home.root);
    }
}
