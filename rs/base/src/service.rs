use crate::home::Home;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const SYSTEMD: &str = include_str!("../../../deploy/systemd/tenon.service");
const LAUNCHD: &str = include_str!("../../../deploy/launchd/com.tenon.base.plist");
pub const UNIT: &str = "tenon.service";
pub const AGENT: &str = "com.tenon.base.plist";

/// The shipped templates with the two placeholders filled in. One source of
/// truth: `deploy/` is what a human copies by hand and what this writes.
pub fn render(template: &str, bin: &Path, home: &Path) -> String {
    template
        .replace("@TENON_BIN@", &bin.display().to_string())
        .replace("@TENON_HOME@", &home.display().to_string())
}

pub fn unit(bin: &Path, home: &Path) -> String {
    render(SYSTEMD, bin, home)
}

pub fn agent(bin: &Path, home: &Path) -> String {
    render(LAUNCHD, bin, home)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn systemd_dir() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("systemd/user"))
}

fn launchd_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/LaunchAgents"))
}

/// `tenon install-service --user`: write the unit for this binary and this
/// home, and enable it when the OS has a user service manager to enable it
/// with. It never starts base — a human decides when the barebone comes up.
pub fn install(home: Option<PathBuf>, print: bool) -> Result<i32> {
    let home = Home::resolve(home)?;
    let bin = std::env::current_exe().context("locate the tenon binary")?;
    let macos = cfg!(target_os = "macos");
    let body = match macos {
        true => agent(&bin, &home.root),
        false => unit(&bin, &home.root),
    };
    if print {
        print!("{body}");
        return Ok(0);
    }
    home.scaffold()?;
    let dir = match macos {
        true => launchd_dir(),
        false => systemd_dir(),
    };
    let Some(dir) = dir else {
        eprintln!("tenon: HOME is not set; write this unit yourself:\n{body}");
        return Ok(1);
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let name = match macos {
        true => AGENT,
        false => UNIT,
    };
    let path = dir.join(name);
    std::fs::write(&path, &body).with_context(|| format!("write {}", path.display()))?;
    println!("tenon: wrote {}", path.display());
    match macos {
        true => println!(
            "tenon: enable it with\n  launchctl load -w {}\n  launchctl start com.tenon.base",
            path.display()
        ),
        false => enable(&path)?,
    }
    Ok(0)
}

fn enable(path: &Path) -> Result<()> {
    let Some(systemctl) = which("systemctl") else {
        println!(
            "tenon: no systemctl on PATH. Enable it yourself with\n  \
             systemctl --user daemon-reload\n  systemctl --user enable --now {UNIT}"
        );
        return Ok(());
    };
    for args in [
        vec!["--user", "daemon-reload"],
        vec!["--user", "enable", UNIT],
    ] {
        let outcome = std::process::Command::new(&systemctl).args(&args).output();
        match outcome {
            Ok(outcome) if outcome.status.success() => {}
            Ok(outcome) => {
                println!(
                    "tenon: systemctl {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&outcome.stderr).trim()
                );
                println!(
                    "tenon: the unit is at {}; enable it by hand",
                    path.display()
                );
                return Ok(());
            }
            Err(error) => {
                println!("tenon: systemctl {} failed: {error}", args.join(" "));
                return Ok(());
            }
        }
    }
    println!("tenon: enabled {UNIT}. Start it with `systemctl --user start {UNIT}`");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_names_this_binary_this_home_and_restarts_always() {
        let unit = unit(Path::new("/opt/tenon"), Path::new("/srv/tenon-home"));
        assert!(unit.contains("ExecStart=/opt/tenon start --foreground --home /srv/tenon-home"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("KillMode=mixed"));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("@TENON_"));
    }

    #[test]
    fn the_launch_agent_carries_the_same_arguments() {
        let agent = agent(Path::new("/opt/tenon"), Path::new("/srv/tenon-home"));
        assert!(agent.contains("<string>/opt/tenon</string>"));
        assert!(agent.contains("<string>--foreground</string>"));
        assert!(agent.contains("<string>/srv/tenon-home</string>"));
        assert!(agent.contains("<key>KeepAlive</key>"));
        assert!(!agent.contains("@TENON_"));
    }
}
