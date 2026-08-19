use crate::proc;
use anyhow::{bail, Context, Result};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const ROOTFS: &str = "rootfs";
const PULL_TIMEOUT: Duration = Duration::from_secs(600);
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

/// `<TENON_HOME|~/.tenon>/images`. Base hands the real path down in the spec;
/// this is what a bare `tenon sandbox image pull` falls back to.
pub fn default_dir() -> PathBuf {
    let home = std::env::var("TENON_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".tenon")))
        .unwrap_or_else(|| PathBuf::from(".tenon"));
    home.join("images")
}

fn in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn run(program: &str, args: &[&str], timeout: Duration) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    let outcome = proc::run(command, timeout)?;
    if outcome.status != 0 {
        bail!(
            "{program} {} exited {}: {}",
            args.join(" "),
            outcome.status,
            String::from_utf8_lossy(&outcome.stderr).trim()
        );
    }
    Ok(())
}

fn stdout(program: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut command = Command::new(program);
    command.args(args);
    let outcome = proc::run(command, timeout)?;
    if outcome.status != 0 {
        bail!(
            "{program} {} exited {}: {}",
            args.join(" "),
            outcome.status,
            String::from_utf8_lossy(&outcome.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&outcome.stdout).trim().to_string())
}

/// Unpacks an OCI image into `<images>/<name>/rootfs`, the directory the krun
/// backend hands to `krun_set_root`. Three engines, in order: podman, docker,
/// then skopeo + umoci. The flattened export is deliberate — a microVM root is
/// a plain directory tree, not a layered store — and the unpack goes to a temp
/// directory that is renamed into place, so an interrupted pull never leaves a
/// half-image behind for a boot to find.
pub fn pull(images: &Path, reference: &str, name: &str) -> Result<PathBuf> {
    let target = images.join(name);
    let staging = images.join(format!(".{name}.staging"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(staging.join(ROOTFS))
        .with_context(|| format!("create {}", staging.join(ROOTFS).display()))?;
    let outcome = match engine() {
        Some(cli) => export(&cli, reference, &staging.join(ROOTFS)),
        None => umoci(reference, &staging),
    };
    if let Err(error) = outcome {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    for dir in ["workspace", "usr/local/bin", "tmp", "proc", "sys", "dev"] {
        let _ = std::fs::create_dir_all(staging.join(ROOTFS).join(dir));
    }
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&staging, &target)
        .with_context(|| format!("move {} into {}", staging.display(), target.display()))?;
    Ok(target.join(ROOTFS))
}

fn engine() -> Option<String> {
    ["podman", "docker"]
        .into_iter()
        .find(|name| in_path(name).is_some())
        .map(str::to_string)
}

fn export(cli: &str, reference: &str, rootfs: &Path) -> Result<()> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let name = format!("tenon-image-{suffix}");
    run(cli, &["pull", reference], PULL_TIMEOUT)?;
    let id = stdout(
        cli,
        &["create", "--name", &name, reference, "/bin/true"],
        STEP_TIMEOUT,
    )?;
    let tar = rootfs
        .parent()
        .unwrap_or(rootfs)
        .join(format!("{name}.tar"));
    let exported = run(
        cli,
        &["export", "-o", &tar.display().to_string(), &name],
        PULL_TIMEOUT,
    );
    let _ = run(cli, &["rm", "-f", &name], STEP_TIMEOUT);
    exported.with_context(|| format!("export {id}"))?;
    let untarred = run(
        "tar",
        &[
            "-xf",
            &tar.display().to_string(),
            "-C",
            &rootfs.display().to_string(),
        ],
        PULL_TIMEOUT,
    );
    let _ = std::fs::remove_file(&tar);
    untarred
}

fn umoci(reference: &str, staging: &Path) -> Result<()> {
    if in_path("skopeo").is_none() || in_path("umoci").is_none() {
        bail!("no image engine: install podman, docker, or skopeo + umoci");
    }
    let layout = staging.join("oci");
    run(
        "skopeo",
        &[
            "copy",
            &format!("docker://{reference}"),
            &format!("oci:{}:tenon", layout.display()),
        ],
        PULL_TIMEOUT,
    )?;
    let bundle = staging.join("bundle");
    run(
        "umoci",
        &[
            "unpack",
            "--rootless",
            "--image",
            &format!("{}:tenon", layout.display()),
            &bundle.display().to_string(),
        ],
        PULL_TIMEOUT,
    )?;
    let _ = std::fs::remove_dir_all(staging.join(ROOTFS));
    std::fs::rename(bundle.join(ROOTFS), staging.join(ROOTFS))
        .context("move the umoci rootfs into place")?;
    let _ = std::fs::remove_dir_all(&layout);
    let _ = std::fs::remove_dir_all(&bundle);
    Ok(())
}

/// Puts the host's own `tenon` binary inside the guest root, the way the oci
/// backend bind-mounts it. A VM has no bind mounts to spare for it: the root is
/// a directory the host owns, so a copy is the mount. Copied only when the
/// bytes differ, since every env boot calls this.
pub fn install_binary(rootfs: &Path, binary: &Path, guest: &str) -> Result<()> {
    if !binary.is_file() {
        return Ok(());
    }
    let target = rootfs.join(guest.trim_start_matches('/'));
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let same = match (std::fs::metadata(binary), std::fs::metadata(&target)) {
        (Ok(from), Ok(to)) => {
            from.len() == to.len()
                && from.modified().ok() == to.modified().ok()
                && to.permissions().mode() & 0o111 != 0
        }
        _ => false,
    };
    if same {
        return Ok(());
    }
    let _ = std::fs::remove_file(&target);
    std::fs::copy(binary, &target)
        .with_context(|| format!("copy {} into {}", binary.display(), target.display()))?;
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod {}", target.display()))?;
    let _ = filetime(binary, &target);
    let _ = std::fs::create_dir_all(rootfs.join("workspace"));
    Ok(())
}

fn filetime(from: &Path, to: &Path) -> Result<()> {
    let modified = std::fs::metadata(from)?.modified()?;
    std::fs::File::options()
        .write(true)
        .open(to)?
        .set_modified(modified)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_directory_follows_tenon_home() {
        let dir = std::env::temp_dir().join("tenon-images-home");
        // SAFETY: single-threaded test, and the variable is restored below.
        let previous = std::env::var("TENON_HOME").ok();
        unsafe { std::env::set_var("TENON_HOME", &dir) };
        assert_eq!(default_dir(), dir.join("images"));
        match previous {
            Some(value) => unsafe { std::env::set_var("TENON_HOME", value) },
            None => unsafe { std::env::remove_var("TENON_HOME") },
        }
    }

    #[test]
    fn installing_the_binary_makes_it_executable_and_is_idempotent() {
        let root = std::env::temp_dir().join(format!("tenon-rootfs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("host-tenon");
        std::fs::write(&binary, b"#!/bin/sh\n").unwrap();
        install_binary(&root, &binary, "/usr/local/bin/tenon").unwrap();
        let target = root.join("usr/local/bin/tenon");
        assert!(target.is_file());
        assert_ne!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o111,
            0
        );
        install_binary(&root, &binary, "/usr/local/bin/tenon").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"#!/bin/sh\n");
        assert!(root.join("workspace").is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_engine_names_all_three_ways_to_get_one() {
        let staging = std::env::temp_dir().join("tenon-umoci-none");
        let error = match (in_path("skopeo"), in_path("umoci")) {
            (Some(_), Some(_)) => return,
            _ => umoci("alpine:3", &staging).unwrap_err().to_string(),
        };
        assert!(error.contains("podman"), "{error}");
        assert!(error.contains("umoci"), "{error}");
    }
}
