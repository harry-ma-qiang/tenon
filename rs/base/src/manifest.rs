use crate::home::Home;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const FILE: &str = "manifest.json";

fn hex(sum: impl AsRef<[u8]>) -> String {
    sum.as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn file_hash(path: &Path) -> Option<String> {
    std::fs::read(path)
        .ok()
        .map(|bytes| hex(Sha256::digest(&bytes)))
}

/// One hash over a whole directory: every file's path and bytes, in sorted
/// order, so a renamed or added profile changes it as much as an edited one.
pub fn tree_hash(dir: &Path) -> String {
    let mut hasher = Sha256::new();
    let mut files = Vec::new();
    walk(dir, dir, &mut files);
    files.sort();
    for (relative, path) in files {
        hasher.update(relative.as_bytes());
        hasher.update([0u8]);
        if let Ok(bytes) = std::fs::read(&path) {
            hasher.update(&bytes);
        }
        hasher.update([0u8]);
    }
    hex(hasher.finalize())
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
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            into.push((relative, path));
        }
    }
}

/// The installed plugin manifests: `<home>/plugins/<name>@<version>/manifest.json`,
/// the same directory the loader resolves profile names against.
pub fn plugins(dir: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path().join(FILE);
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let name = manifest["name"].as_str().unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }
        rows.push(json!({
            "name": name,
            "version": manifest["version"].as_str().unwrap_or_default(),
            "hash": manifest["hash"].as_str().unwrap_or_default(),
            "manifest_hash": file_hash(&path),
        }));
    }
    rows.sort_by(|a, b| a["name"].to_string().cmp(&b["name"].to_string()));
    rows
}

fn release_version(release: &Path) -> String {
    let dir = release
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    format!("{}/{dir}", env!("CARGO_PKG_VERSION"))
}

pub fn path(home: &Home) -> PathBuf {
    home.lkg().join(FILE)
}

/// Written at every LKG promotion, over the copies that were just taken: what
/// is pinned, and the hashes `tenon rollback` checks before restoring any of
/// it.
pub fn write(home: &Home, release: &Path) -> Result<Value> {
    let lkg = home.lkg();
    let state = lkg.join("state.sqlite");
    let manifest = json!({
        "at": tenon_storage::now(),
        "config_hash": file_hash(&lkg.join("config.yml")),
        "profile_hash": tree_hash(&lkg.join("profiles")),
        "release_version": release_version(release),
        "plugins": plugins(&home.plugins_dir()),
        "state_copy": {
            "path": "state.sqlite",
            "sha256": file_hash(&state),
            "bytes": std::fs::metadata(&state).map(|meta| meta.len()).unwrap_or(0),
        },
    });
    let body = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(path(home), body).with_context(|| format!("write {}", path(home).display()))?;
    Ok(manifest)
}

pub fn read(home: &Home) -> Result<Value> {
    let file = path(home);
    let body =
        std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    Ok(serde_json::from_str(&body)?)
}

/// Recomputes every hash the manifest pinned and names what moved. The LKG
/// copies must match what was promoted, and the installed plugins must match
/// what was pinned; either kind of drift is a refusal to restore.
pub fn verify(home: &Home, manifest: &Value) -> Vec<Value> {
    let lkg = home.lkg();
    let mut differs = Vec::new();
    let checks = [
        (
            "config.yml",
            manifest["config_hash"].as_str().map(str::to_string),
            file_hash(&lkg.join("config.yml")),
        ),
        (
            "profiles/",
            manifest["profile_hash"].as_str().map(str::to_string),
            Some(tree_hash(&lkg.join("profiles"))),
        ),
        (
            "state.sqlite",
            manifest["state_copy"]["sha256"]
                .as_str()
                .map(str::to_string),
            file_hash(&lkg.join("state.sqlite")),
        ),
    ];
    for (what, pinned, found) in checks {
        if pinned != found {
            differs.push(json!({
                "what": what,
                "pinned": pinned,
                "found": found,
            }));
        }
    }
    let installed = plugins(&home.plugins_dir());
    for pinned in manifest["plugins"].as_array().cloned().unwrap_or_default() {
        let found = installed
            .iter()
            .find(|row| row["name"] == pinned["name"] && row["version"] == pinned["version"]);
        match found {
            None => differs.push(json!({
                "what": format!("plugin {}@{}", pinned["name"], pinned["version"]),
                "pinned": pinned["hash"],
                "found": Value::Null,
            })),
            Some(row) if row["hash"] != pinned["hash"] => differs.push(json!({
                "what": format!("plugin {}@{}", pinned["name"], pinned["version"]),
                "pinned": pinned["hash"],
                "found": row["hash"],
            })),
            Some(_row) => {}
        }
    }
    differs
}

fn restore_file(from: &Path, into: &Path) -> Result<()> {
    if !from.is_file() {
        return Ok(());
    }
    std::fs::copy(from, into)
        .with_context(|| format!("restore {}", into.display()))
        .map(|_bytes| ())
}

/// `tenon rollback`: verify, then put the pinned config, profiles and state
/// copy back. Base must be down — restoring `state.sqlite` under a live
/// writer would corrupt exactly what is being rescued.
pub fn rollback(home: &Home, force: bool) -> Result<Value> {
    if home.ready_file().is_file() {
        let pid = std::fs::read_to_string(home.ready_file()).unwrap_or_default();
        bail!("base is running (pid {}); stop it first", pid.trim());
    }
    let manifest = read(home)?;
    let differs = verify(home, &manifest);
    if !differs.is_empty() && !force {
        let lines: Vec<String> = differs
            .iter()
            .map(|row| {
                format!(
                    "  {}: pinned {}, found {}",
                    row["what"].as_str().unwrap_or_default(),
                    row["pinned"],
                    row["found"]
                )
            })
            .collect();
        bail!(
            "the lkg manifest does not match what is on disk:\n{}\nrefusing to roll back \
             (--force overrides)",
            lines.join("\n")
        );
    }
    let lkg = home.lkg();
    restore_file(&lkg.join("config.yml"), &home.config_file())?;
    restore_file(&lkg.join("state.sqlite"), &home.state_file())?;
    for stale in ["state.sqlite-wal", "state.sqlite-shm"] {
        let _ = std::fs::remove_file(home.root.join(stale));
    }
    let profiles = lkg.join("profiles");
    if profiles.is_dir() {
        let _ = std::fs::remove_dir_all(home.profiles());
        crate::home::copy_tree(&profiles, &home.profiles())?;
    }
    Ok(json!({
        "ok": true,
        "home": home.root,
        "restored": ["config.yml", "profiles/", "state.sqlite"],
        "forced": force,
        "differs": differs,
        "manifest": manifest,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(name: &str) -> Home {
        let root = std::env::temp_dir().join(format!("tenon-lkg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = Home { root };
        home.scaffold().expect("scaffold");
        std::fs::write(home.config_file(), "root_env: root\n").expect("config");
        std::fs::write(home.state_file(), b"not really sqlite").expect("state");
        std::fs::create_dir_all(home.profiles().join("root")).expect("profiles");
        std::fs::write(home.profiles().join("root/tenon.yml"), "[]\n").expect("profile");
        home
    }

    fn plugin(home: &Home, name: &str, version: &str, hash: &str) {
        let dir = home.plugins_dir().join(format!("{name}@{version}"));
        std::fs::create_dir_all(&dir).expect("plugin dir");
        let manifest = json!({"name": name, "version": version, "hash": hash,
                              "cmd": "/bin/true", "protocol": "wire/1"});
        std::fs::write(dir.join(FILE), manifest.to_string()).expect("manifest");
    }

    #[test]
    fn a_promotion_pins_the_copies_and_the_installed_plugins() {
        let home = home("write");
        plugin(&home, "echo", "1.0.0", "sha256:echo");
        home.promote_lkg().expect("promote");
        let manifest = write(&home, Path::new("/opt/tenon_beam")).expect("write");
        assert!(manifest["release_version"]
            .as_str()
            .expect("version")
            .ends_with("/tenon_beam"));
        assert_eq!(manifest["plugins"][0]["name"], "echo");
        assert_eq!(manifest["state_copy"]["path"], "state.sqlite");
        assert!(verify(&home, &manifest).is_empty());
        assert_eq!(
            read(&home).expect("read")["config_hash"],
            manifest["config_hash"]
        );
        let _ = std::fs::remove_dir_all(&home.root);
    }

    #[test]
    fn verify_names_every_drift_and_rollback_refuses_until_forced() {
        let home = home("verify");
        plugin(&home, "echo", "1.0.0", "sha256:echo");
        home.promote_lkg().expect("promote");
        let manifest = write(&home, Path::new("/opt/tenon_beam")).expect("write");

        std::fs::write(home.lkg().join("config.yml"), "root_env: tampered\n").expect("tamper");
        plugin(&home, "echo", "1.0.0", "sha256:other");
        let differs = verify(&home, &manifest);
        let what: Vec<String> = differs
            .iter()
            .map(|row| row["what"].as_str().unwrap_or_default().to_string())
            .collect();
        assert!(what.contains(&"config.yml".to_string()), "{what:?}");
        assert!(what.iter().any(|row| row.contains("plugin")), "{what:?}");

        let error = rollback(&home, false).expect_err("refused").to_string();
        assert!(error.contains("does not match"), "{error}");
        assert!(error.contains("config.yml"), "{error}");

        std::fs::write(home.config_file(), "root_env: live\n").expect("live config");
        let done = rollback(&home, true).expect("forced");
        assert_eq!(done["forced"], true);
        let restored = std::fs::read_to_string(home.config_file()).expect("read config");
        assert_eq!(restored, "root_env: tampered\n");
        let _ = std::fs::remove_dir_all(&home.root);
    }

    #[test]
    fn rollback_refuses_while_base_is_running() {
        let home = home("running");
        home.promote_lkg().expect("promote");
        write(&home, Path::new("/opt/tenon_beam")).expect("write");
        std::fs::write(home.ready_file(), "4242").expect("ready");
        let error = rollback(&home, true).expect_err("refused").to_string();
        assert!(error.contains("4242"), "{error}");
        let _ = std::fs::remove_dir_all(&home.root);
    }
}
