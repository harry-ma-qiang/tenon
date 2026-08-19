use crate::home::Home;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The contract version base asks the shipped suite for. Bumping it is a human
/// decision: a kernel whose wire or API changed is a new contract, and RFC
/// section 10 puts that behind a human, not behind an agent's upgrade.
pub const CONTRACT: &str = "1";

const TIMEOUT: Duration = Duration::from_secs(120);

/// What the suite in the release answered: one row per contract point, plus
/// the two counts and the beam it ran against.
#[derive(Debug, Clone)]
pub struct Report {
    pub ok: bool,
    pub contract: String,
    pub beam: String,
    pub passed: i64,
    pub failed: Vec<(String, String)>,
    pub raw: Value,
}

impl Report {
    pub fn reason(&self) -> String {
        if self.ok {
            return format!("{} contract points passed", self.passed);
        }
        let rows: Vec<String> = self
            .failed
            .iter()
            .map(|(name, error)| format!("{name}: {error}"))
            .collect();
        match rows.is_empty() {
            true => "the contract suite failed without naming a point".to_string(),
            false => format!("contract points failed: {}", rows.join("; ")),
        }
    }
}

/// The `tenon.beam` a release ships, which is the default subject of the check
/// and the file a kernel upgrade replaces.
pub fn shipped_beam(release: &Path) -> Result<PathBuf> {
    let lib = release.join("lib");
    let entries = std::fs::read_dir(&lib).with_context(|| format!("read {}", lib.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "tenon" || name.starts_with("tenon-") {
            let beam = entry.path().join("ebin/tenon.beam");
            if beam.is_file() {
                return Ok(beam);
            }
        }
    }
    bail!("{} ships no lib/tenon-*/ebin/tenon.beam", release.display())
}

/// Runs the contract suite that lives inside the release against `beam`. The
/// suite is a release artifact on purpose: an installed machine has no
/// development tree, no mix and no test files, and still has to be able to
/// prove a candidate kernel keeps the contract.
pub fn run(release: &Path, beam: Option<&Path>) -> Result<Report> {
    let binary = release.join("bin/tenon_beam");
    if !binary.is_file() {
        bail!("{} holds no bin/tenon_beam", release.display());
    }
    let subject = match beam {
        Some(path) => path.to_path_buf(),
        None => shipped_beam(release)?,
    };
    if !subject.is_file() {
        bail!("{} is not a file", subject.display());
    }
    let mut command = Command::new(&binary);
    command
        .arg("eval")
        .arg("Tenon.Beam.Check.main()")
        .env("TENON_CHECK_BEAM", &subject)
        .env("TENON_KERNEL_CONTRACT", CONTRACT)
        .env_remove("TENON_ROLE");
    let output = timed(command)?;
    let text = String::from_utf8_lossy(&output.0).to_string();
    let Some(raw) = document(&text) else {
        bail!(
            "the contract suite printed no report:\n{}\n{}",
            text.trim(),
            String::from_utf8_lossy(&output.1).trim()
        );
    };
    Ok(report(raw))
}

fn report(raw: Value) -> Report {
    let failed: Vec<(String, String)> = raw["points"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| row["ok"] != Value::Bool(true))
                .map(|row| {
                    (
                        row["name"].as_str().unwrap_or("?").to_string(),
                        row["error"].as_str().unwrap_or("failed").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Report {
        ok: raw["ok"] == Value::Bool(true) && failed.is_empty(),
        contract: raw["contract"].as_str().unwrap_or_default().to_string(),
        beam: raw["beam"].as_str().unwrap_or_default().to_string(),
        passed: raw["passed"].as_i64().unwrap_or(0),
        failed,
        raw,
    }
}

/// The suite prints one JSON line; anything the VM logged around it is noise,
/// so the last parseable line is the report.
fn document(text: &str) -> Option<Value> {
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|value| value.get("contract").is_some())
}

fn timed(mut command: Command) -> Result<(Vec<u8>, Vec<u8>)> {
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start the contract suite")?;
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!("the contract suite did not finish inside {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output()?;
    Ok((output.stdout, output.stderr))
}

/// `tenon check kernel`: resolve the release the same way `start` does, run the
/// suite, print the report and exit non-zero when a point failed.
pub fn command(
    home: Option<PathBuf>,
    beam: Option<PathBuf>,
    release_dir: Option<PathBuf>,
    payload: Option<&'static [u8]>,
    version: &'static str,
) -> Result<i32> {
    let home = Home::resolve(home)?;
    home.scaffold()?;
    let release = crate::release::resolve(&home, release_dir.as_deref(), payload, version)?;
    let report = run(&release, beam.as_deref())?;
    println!("{}", serde_json::to_string_pretty(&report.raw)?);
    match report.ok {
        true => {
            println!(
                "tenon check kernel: contract {} ok, {} points, {}",
                report.contract, report.passed, report.beam
            );
            Ok(0)
        }
        false => {
            eprintln!("tenon check kernel: {}", report.reason());
            Ok(1)
        }
    }
}
