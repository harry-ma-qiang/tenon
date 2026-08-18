use crate::home::Home;
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn resolve(
    home: &Home,
    cli: Option<&Path>,
    payload: Option<&'static [u8]>,
    version: &str,
) -> Result<PathBuf> {
    if let Some(dir) = cli {
        return verify(dir.to_path_buf());
    }
    if let Some(dir) = std::env::var_os("TENON_RELEASE_DIR") {
        return verify(PathBuf::from(dir));
    }
    let payload = payload.context(
        "no beam release: pass --release-dir, set TENON_RELEASE_DIR, \
         or build the binary with TENON_RELEASE_TAR set to a release tarball",
    )?;
    extract(home, payload, version)
}

fn verify(dir: PathBuf) -> Result<PathBuf> {
    if dir.join("bin/tenon_beam").is_file() {
        return Ok(dir);
    }
    bail!("{} holds no bin/tenon_beam", dir.display())
}

fn extract(home: &Home, payload: &'static [u8], version: &str) -> Result<PathBuf> {
    let tag = format!("{version}-{}", digest(payload));
    let into = home.erts().join(&tag);
    if into.join("bin/tenon_beam").is_file() {
        return Ok(into);
    }
    let staging = home.erts().join(format!(".{tag}.staging"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let mut archive = tar::Archive::new(GzDecoder::new(payload));
    archive.set_preserve_permissions(true);
    archive
        .unpack(&staging)
        .context("unpack the embedded beam release")?;
    let root = release_root(&staging)?;
    let _ = std::fs::remove_dir_all(&into);
    std::fs::rename(&root, &into)
        .with_context(|| format!("install the release into {}", into.display()))?;
    let _ = std::fs::remove_dir_all(&staging);
    verify(into)
}

fn release_root(staging: &Path) -> Result<PathBuf> {
    if staging.join("bin/tenon_beam").is_file() {
        return Ok(staging.to_path_buf());
    }
    for entry in std::fs::read_dir(staging)? {
        let path = entry?.path();
        if path.join("bin/tenon_beam").is_file() {
            return Ok(path);
        }
    }
    bail!("the embedded payload holds no bin/tenon_beam")
}

fn digest(payload: &[u8]) -> String {
    let sum = Sha256::digest(payload);
    sum.iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
