use crate::home::{copy_tree, Home};
use crate::manifest;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const MANIFEST: &str = "backup.json";

/// A consistent copy of every durable host file into `dest`: each SQLite state
/// file through the online snapshot path (coherent even while base writes),
/// plus `config.yml`, `profiles/` and the LKG manifest as plain copies. The
/// release (`erts/`), the sockets and pids (`run/`) and the rebuildable warm
/// segments (`derived/`) are left out on purpose. A `backup.json` records the
/// tenon version, the moment, the env list and every file's sha256 and size.
pub fn run(home: &Home, dest: &Path) -> Result<Value> {
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    let barebone = home.state_file();
    if !barebone.is_file() {
        bail!("no state.sqlite under {}", home.root.display());
    }
    tenon_storage::backup_file(&barebone, &dest.join("state.sqlite"))?;
    let mut envs = Vec::new();
    for (env, path) in env_state_files(home) {
        tenon_storage::backup_file(&path, &dest.join(format!("state-{env}.sqlite")))?;
        envs.push(env);
    }
    copy_plain(&home.config_file(), &dest.join("config.yml"))?;
    if home.profiles().is_dir() {
        let _ = std::fs::remove_dir_all(dest.join("profiles"));
        copy_tree(&home.profiles(), &dest.join("profiles"))?;
    }
    let lkg_manifest = home.lkg().join(manifest::FILE);
    if lkg_manifest.is_file() {
        copy_plain(&lkg_manifest, &dest.join("lkg").join(manifest::FILE))?;
    }
    let files = hash_tree(dest);
    let manifest = json!({
        "tenon_version": env!("CARGO_PKG_VERSION"),
        "at": tenon_storage::now(),
        "envs": envs,
        "files": files,
    });
    std::fs::write(
        dest.join(MANIFEST),
        serde_json::to_string_pretty(&manifest)?,
    )
    .with_context(|| format!("write {}", dest.join(MANIFEST).display()))?;
    Ok(json!({
        "ok": true,
        "dir": dest,
        "envs": envs,
        "files": files.len(),
        "at": manifest["at"],
    }))
}

/// Verify then replace: refuses to run over a live base, checks every file's
/// sha256 against `backup.json` and names any that differ, snapshots the
/// pre-restore state to `.restore-bak-<ts>`, then puts the state files, config,
/// profiles and LKG manifest back in place.
pub fn restore(home: &Home, src: &Path) -> Result<Value> {
    if home.ready_file().is_file() {
        let pid = std::fs::read_to_string(home.ready_file()).unwrap_or_default();
        bail!("base is running (pid {}); stop it first", pid.trim());
    }
    let manifest_path = src.join(MANIFEST);
    let body = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: Value = serde_json::from_str(&body)
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    let files = manifest["files"].as_array().cloned().unwrap_or_default();
    if files.is_empty() {
        bail!("{} lists no files", manifest_path.display());
    }
    let differs = verify(src, &files);
    if !differs.is_empty() {
        bail!(
            "the backup does not match {}:\n{}\nrefusing to restore",
            MANIFEST,
            differs.join("\n")
        );
    }
    let stamp = tenon_storage::now();
    let bak = home.root.join(format!(".restore-bak-{stamp}"));
    save_current(home, &bak)?;
    let restored = apply(home, src, &files)?;
    Ok(json!({
        "ok": true,
        "home": home.root,
        "from": src,
        "restored": restored,
        "pre_restore_backup": bak,
        "tenon_version": manifest["tenon_version"],
    }))
}

fn verify(src: &Path, files: &[Value]) -> Vec<String> {
    let mut differs = Vec::new();
    for file in files {
        let Some(rel) = file["path"].as_str() else {
            continue;
        };
        let want = file["sha256"].as_str().unwrap_or_default();
        match manifest::file_hash(&src.join(rel)) {
            None => differs.push(format!("  {rel}: missing")),
            Some(got) if got != want => {
                differs.push(format!("  {rel}: sha256 {got}, expected {want}"))
            }
            Some(_) => {}
        }
    }
    differs
}

fn apply(home: &Home, src: &Path, files: &[Value]) -> Result<Vec<String>> {
    for stale in ["state.sqlite-wal", "state.sqlite-shm"] {
        let _ = std::fs::remove_file(home.root.join(stale));
    }
    for (env, _path) in env_state_files(home) {
        let _ = std::fs::remove_file(home.env_state_file(&env).with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(home.env_state_file(&env).with_extension("sqlite-shm"));
    }
    let _ = std::fs::remove_dir_all(home.profiles());
    let mut restored = Vec::new();
    for file in files {
        let Some(rel) = file["path"].as_str() else {
            continue;
        };
        copy_plain(&src.join(rel), &home.root.join(rel))?;
        restored.push(rel.to_string());
    }
    restored.sort();
    Ok(restored)
}

fn save_current(home: &Home, bak: &Path) -> Result<()> {
    std::fs::create_dir_all(bak).with_context(|| format!("create {}", bak.display()))?;
    copy_plain(&home.state_file(), &bak.join("state.sqlite"))?;
    for (env, path) in env_state_files(home) {
        copy_plain(&path, &bak.join(format!("state-{env}.sqlite")))?;
    }
    copy_plain(&home.config_file(), &bak.join("config.yml"))?;
    if home.profiles().is_dir() {
        copy_tree(&home.profiles(), &bak.join("profiles"))?;
    }
    if home.lkg().is_dir() {
        copy_tree(&home.lkg(), &bak.join("lkg"))?;
    }
    Ok(())
}

/// The env names this home has a `state-<env>.sqlite` for, barebone excluded.
fn env_state_files(home: &Home) -> Vec<(String, PathBuf)> {
    let mut rows = Vec::new();
    let Ok(entries) = std::fs::read_dir(&home.root) else {
        return rows;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("state-") else {
            continue;
        };
        let Some(env) = rest.strip_suffix(".sqlite") else {
            continue;
        };
        rows.push((env.to_string(), entry.path()));
    }
    rows.sort();
    rows
}

/// Every file under `dir` except the manifest itself, each with its sha256 and
/// byte length, sorted by relative path so a backup hashes deterministically.
fn hash_tree(dir: &Path) -> Vec<Value> {
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    files
        .into_iter()
        .filter(|(rel, _)| rel != MANIFEST)
        .map(|(rel, path)| {
            let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
            json!({
                "path": rel,
                "sha256": manifest::file_hash(&path).unwrap_or_default(),
                "bytes": bytes,
            })
        })
        .collect()
}

fn walk(root: &Path, dir: &Path, into: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, into);
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            into.push((rel, path));
        }
    }
}

fn copy_plain(from: &Path, into: &Path) -> Result<()> {
    if !from.is_file() {
        return Ok(());
    }
    if let Some(parent) = into.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, into)
        .with_context(|| format!("copy {} to {}", from.display(), into.display()))?;
    Ok(())
}
