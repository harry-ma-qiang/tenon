use crate::base::Base;
use crate::config::ProbeEntry;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// What base hands the guardian node as `TENON_GUARDIAN_PROBES`, and what it
/// refused to hand it. Extra probes are executables under `<home>/probes/`
/// that base checks against `probes.extra` in its own config before the
/// guardian ever sees them: humans edit base's config, so the sha256 there is
/// the signature.
#[derive(Debug, Default, Clone)]
pub struct Approved {
    pub paths: Vec<PathBuf>,
    pub rejected: Vec<Value>,
}

impl Approved {
    pub fn joined(&self) -> String {
        self.paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<String>>()
            .join(":")
    }
}

fn hash(path: &Path) -> Option<String> {
    Some(crate::hash::sha256(std::fs::read(path).ok()?))
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn check(dir: &Path, entry: &ProbeEntry) -> Result<PathBuf, String> {
    if entry.file.is_empty() {
        return Err("probes.extra needs a file".to_string());
    }
    if entry.file.contains('/') || entry.file.contains("..") {
        return Err(format!(
            "{} is not a plain file name under {}",
            entry.file,
            dir.display()
        ));
    }
    let path = dir.join(&entry.file);
    if !path.is_file() {
        return Err(format!("{} does not exist", path.display()));
    }
    if !executable(&path) {
        return Err(format!("{} is not executable", path.display()));
    }
    let found = hash(&path).ok_or_else(|| format!("{} is unreadable", path.display()))?;
    let want = entry.sha256.trim().to_ascii_lowercase();
    if want.is_empty() {
        return Err(format!("{} has no sha256 in the config", entry.file));
    }
    if want != found {
        return Err(format!("sha256 is {found}, the config says {want}"));
    }
    Ok(path)
}

pub fn approve(dir: &Path, entries: &[ProbeEntry]) -> Approved {
    let mut approved = Approved::default();
    for entry in entries {
        match check(dir, entry) {
            Ok(path) => approved.paths.push(path),
            Err(reason) => approved
                .rejected
                .push(json!({"file": entry.file, "reason": reason})),
        }
    }
    approved
}

impl Base {
    /// Runs once per boot, before the guardian node is started: the approved
    /// list travels in the guardian's environment and the refusals are events
    /// a human can read.
    pub fn load_probes(&mut self) {
        let dir = self.home.probes_dir();
        let approved = approve(&dir, &self.config.probes.extra);
        for rejected in &approved.rejected {
            self.emit("probes.rejected", None, rejected.clone());
        }
        if !approved.paths.is_empty() {
            self.emit(
                "probes.loaded",
                None,
                json!({"count": approved.paths.len(), "dir": dir}),
            );
        }
        self.probes = approved;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str, mode: u32) -> String {
        let path = dir.join(name);
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&path, body).expect("write probe");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        hash(&path).expect("hash")
    }

    fn entry(file: &str, sha256: &str) -> ProbeEntry {
        ProbeEntry {
            file: file.to_string(),
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn only_a_listed_file_whose_hash_matches_is_approved() {
        let dir = std::env::temp_dir().join(format!("tenon-probes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let good = write(&dir, "good.sh", "#!/bin/sh\nexit 0\n", 0o755);
        let plain = write(&dir, "plain.sh", "#!/bin/sh\nexit 0\n", 0o644);

        let approved = approve(
            &dir,
            &[
                entry("good.sh", &good.to_ascii_uppercase()),
                entry("good.sh", "0000"),
                entry("plain.sh", &plain),
                entry("missing.sh", &good),
                entry("../good.sh", &good),
                entry("good.sh", ""),
            ],
        );
        assert_eq!(approved.paths, vec![dir.join("good.sh")]);
        assert_eq!(approved.rejected.len(), 5);
        let reasons: Vec<String> = approved
            .rejected
            .iter()
            .map(|row| row["reason"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(reasons[0].contains("sha256 is"), "{reasons:?}");
        assert!(reasons[1].contains("not executable"), "{reasons:?}");
        assert!(reasons[2].contains("does not exist"), "{reasons:?}");
        assert!(reasons[3].contains("plain file name"), "{reasons:?}");
        assert!(reasons[4].contains("no sha256"), "{reasons:?}");
        assert_eq!(approved.joined(), dir.join("good.sh").display().to_string());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
