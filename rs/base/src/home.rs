use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const DEMO_ID: &str = "demo";
const TERM_NAME: &str = "demo:term";
const GUARD_NAME: &str = "demo:guard";

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Demo {
    pub name: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Home {
    pub fn resolve(cli: Option<PathBuf>) -> Result<Self> {
        let root = match cli.or_else(|| std::env::var_os("TENON_HOME").map(PathBuf::from)) {
            Some(path) => path,
            None => {
                let home = std::env::var_os("HOME").context("HOME is not set")?;
                PathBuf::from(home).join(".tenon")
            }
        };
        Ok(Self { root })
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.yml")
    }

    #[cfg(feature = "http")]
    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.yml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.sqlite")
    }

    pub fn profiles(&self) -> PathBuf {
        self.root.join("profiles")
    }

    pub fn profile(&self, env: &str) -> PathBuf {
        self.profiles().join(env).join("tenon.yml")
    }

    pub fn erts(&self) -> PathBuf {
        self.root.join("erts")
    }

    pub fn run(&self) -> PathBuf {
        self.root.join("run")
    }

    /// Where an extra guardian probe must live to be loadable at all; base
    /// checks each one against the sha256 in its own config before the
    /// guardian node is told about it.
    /// Installed plugin versions, `<name>@<version>/manifest.json` each: the
    /// loader's manifest registry source and what the LKG manifest pins.
    pub fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    pub fn probes_dir(&self) -> PathBuf {
        self.root.join("probes")
    }

    pub fn envs_dir(&self) -> PathBuf {
        self.root.join("envs")
    }

    pub fn env_dir(&self, env: &str) -> PathBuf {
        self.envs_dir().join(env)
    }

    pub fn workspace_dir(&self, env: &str) -> PathBuf {
        self.env_dir(env).join("workspace")
    }

    pub fn env_state_file(&self, env: &str) -> PathBuf {
        self.root.join(format!("state-{env}.sqlite"))
    }

    pub fn restore_dir(&self, env: &str) -> PathBuf {
        self.workspace_dir(env).join(".tenon-restore")
    }

    /// One directory per env, holding only that env's gateway socket. The oci
    /// backend bind-mounts this directory into the instance, so a shared
    /// `run/` would put base's front door and every sibling's gateway inside
    /// every sandbox; a per-env directory is what keeps a child env from
    /// reaching its parent's socket at all.
    pub fn gateway_dir(&self, env: &str) -> PathBuf {
        self.run().join(format!("gw-{env}"))
    }

    pub fn gateway_sock(&self, env: &str) -> PathBuf {
        self.gateway_dir(env).join("gateway.sock")
    }

    pub fn gateway_address(&self, env: &str) -> String {
        format!("unix:{}", self.gateway_sock(env).display())
    }

    /// The blue/green candidate's socket, in the **same** directory as the
    /// env's own: that directory is what a sandbox mounts, so the worker
    /// reaches either node without a second mount.
    pub fn green_gateway_sock(&self, env: &str) -> PathBuf {
        self.gateway_dir(env).join("gateway-green.sock")
    }

    pub fn green_gateway_address(&self, env: &str) -> String {
        format!("unix:{}", self.green_gateway_sock(env).display())
    }

    /// Where one upgrade proposal keeps whatever it has to stage on disk: a
    /// candidate release, for now.
    pub fn upgrade_dir(&self, id: i64) -> PathBuf {
        self.root.join("upgrades").join(id.to_string())
    }

    /// The promoted candidate worker of an env, or nothing when the built-in
    /// worker (the LKG fallback) is what base launches.
    pub fn worker_spec_file(&self, env: &str) -> PathBuf {
        self.profiles().join(env).join("worker.json")
    }

    /// The release an env's node A boots from, when a kernel upgrade moved it
    /// off base's own. It lives under `profiles/`, so the LKG copy of the
    /// profiles is what `tenon rollback` puts back.
    pub fn kernel_file(&self, env: &str) -> PathBuf {
        self.profiles().join(env).join("kernel.json")
    }

    pub fn write_kernel_release(&self, env: &str, release: &Path) -> Result<()> {
        let path = self.kernel_file(env);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = json!({"release": release}).to_string();
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn kernel_release(&self, env: &str) -> Option<PathBuf> {
        let body = std::fs::read_to_string(self.kernel_file(env)).ok()?;
        let value: Value = serde_json::from_str(&body).ok()?;
        let path = PathBuf::from(value.get("release")?.as_str()?);
        path.join("bin/tenon_beam").is_file().then_some(path)
    }

    pub fn worker_spec(&self, env: &str) -> Option<Value> {
        let body = std::fs::read_to_string(self.worker_spec_file(env)).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Prepared root filesystems, one directory per image name, shared by every
    /// env: `tenon sandbox image pull` writes them and the krun backend reads
    /// them. Nothing here is per-env, so nothing here is agent-writable.
    pub fn images_dir(&self) -> PathBuf {
        self.root.join("images")
    }

    pub fn sock(&self) -> PathBuf {
        self.run().join("base.sock")
    }

    pub fn log(&self, name: &str) -> PathBuf {
        self.run().join(format!("{name}.log"))
    }

    pub fn ready_file(&self) -> PathBuf {
        self.run().join("base.ready")
    }

    pub fn ready_tmp_file(&self) -> PathBuf {
        self.run().join("base.ready.tmp")
    }

    /// The env's runtime token, owner-readable only: how a runtime a human
    /// starts by hand authenticates its `runtime.register`.
    pub fn runtime_token_file(&self, env: &str) -> PathBuf {
        self.run().join(format!("rt-{env}.token"))
    }

    pub fn lock_file(&self) -> PathBuf {
        self.run().join("base.lock")
    }

    pub fn lkg_state_file(&self) -> PathBuf {
        self.lkg().join("state.sqlite")
    }

    pub fn lkg(&self) -> PathBuf {
        self.root.join("lkg")
    }

    /// The first 12 hex chars of sha256(home path); a short, filesystem-safe id
    /// that ties every sandbox instance this home ever spawns back to it, so a
    /// reap pass never touches a container that belongs to a different home.
    pub fn hash(&self) -> String {
        crate::hash::short(self.root.display().to_string(), 6)
    }

    pub fn scaffold(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.profiles(),
            self.erts(),
            self.run(),
            self.lkg(),
            self.probes_dir(),
            self.plugins_dir(),
        ] {
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        Ok(())
    }

    pub fn prepare(&self, root_env: &str) -> Result<()> {
        self.scaffold()?;
        self.write_profile("guardian", None)?;
        self.write_profile(root_env, demo().as_ref())?;
        Ok(())
    }

    pub fn write_profile(&self, env: &str, demo: Option<&Demo>) -> Result<()> {
        let dir = self.profiles().join(env);
        std::fs::create_dir_all(&dir)?;
        let entries = dir.join("tenon.yml");
        let registry = dir.join("registry.yml");
        if !entries.exists() {
            std::fs::write(&entries, entry_list(demo))?;
        }
        if !registry.exists() {
            std::fs::write(&registry, registry_map(demo))?;
        }
        Ok(())
    }

    /// Everything one env owns on the host, made before its node starts.
    pub fn prepare_env(&self, env: &str) -> Result<()> {
        std::fs::create_dir_all(self.workspace_dir(env))?;
        std::fs::create_dir_all(self.gateway_dir(env))?;
        self.write_harness_default(env)?;
        Ok(())
    }

    /// The env's harness overlay: the provider, the model and the key's
    /// *name* (never the key itself), plus the loop's own knobs. It is L3
    /// config in RFC section 10 terms, so `config.patch` snapshots it first.
    pub fn harness_file(&self, env: &str) -> PathBuf {
        self.profiles().join(env).join("harness.yml")
    }

    pub fn config_snapshots(&self, env: &str) -> PathBuf {
        self.root.join("config-snapshots").join(env)
    }

    pub fn write_harness_default(&self, env: &str) -> Result<()> {
        let path = self.harness_file(env);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, HARNESS_DEFAULT)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn harness_config(&self, env: &str) -> Result<Value> {
        let path = self.harness_file(env);
        if !path.is_file() {
            return Ok(json!({}));
        }
        let body =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let value: Value =
            serde_yaml::from_str(&body).with_context(|| format!("parse {}", path.display()))?;
        Ok(match value.is_object() {
            true => value,
            false => json!({}),
        })
    }

    /// Snapshot, merge, write. The snapshot is a plain copy under
    /// `config-snapshots/<env>/`, which is what makes an agent's own config
    /// change rollback-able without touching the LKG the barebone promotes.
    pub fn patch_harness(&self, env: &str, patch: &Value) -> Result<(PathBuf, Value)> {
        self.write_harness_default(env)?;
        let path = self.harness_file(env);
        let dir = self.config_snapshots(env);
        std::fs::create_dir_all(&dir)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|at| at.as_millis())
            .unwrap_or(0);
        let snapshot = dir.join(format!("harness-{stamp}.yml"));
        copy_file(&path, &snapshot)?;
        let mut config = self.harness_config(env)?;
        merge(&mut config, patch);
        let body = serde_yaml::to_string(&config)?;
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        Ok((snapshot, config))
    }

    /// A child env's profile is its parent's layers plus one patch file of its
    /// own, in loader order: `TENON_PROFILE` carries them separated by `:` and
    /// the loader applies the patch over the parent's entry list.
    pub fn write_overlay(&self, env: &str, overlay: &str) -> Result<PathBuf> {
        let dir = self.profiles().join(env);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("overlay.patch.yml");
        std::fs::write(&path, overlay)?;
        let registry = dir.join("registry.yml");
        if !registry.exists() {
            std::fs::write(&registry, "{}\n")?;
        }
        Ok(path)
    }

    pub fn wipe_workspace(&self, env: &str) -> Result<()> {
        let dir = self.workspace_dir(env);
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(entry.path())?;
                } else {
                    std::fs::remove_file(entry.path())?;
                }
            }
        }
        std::fs::create_dir_all(&dir)?;
        Ok(())
    }

    pub fn promote_lkg(&self) -> Result<()> {
        let lkg = self.lkg();
        std::fs::create_dir_all(&lkg)?;
        copy_file(&self.config_file(), &lkg.join("config.yml"))?;
        copy_file(&self.state_file(), &lkg.join("state.sqlite"))?;
        copy_tree(&self.profiles(), &lkg.join("profiles"))?;
        Ok(())
    }

    pub fn restore_env(&self, env: &str) -> Result<bool> {
        let from = self.lkg().join("profiles").join(env);
        if !from.is_dir() {
            return Ok(false);
        }
        let into = self.profiles().join(env);
        let _ = std::fs::remove_dir_all(&into);
        copy_tree(&from, &into)?;
        Ok(true)
    }
}

const HARNESS_DEFAULT: &str = "\
llm:
  provider: openai
  base_url: https://api.deepseek.com
  model: deepseek-v4-flash
  api_key_env: DEEPSEEK_API_KEY
max_steps: 8
approval: deny
";

/// Object keys merge, everything else replaces: the loader's patch semantics
/// for one file rather than for an entry list.
pub fn merge(into: &mut Value, patch: &Value) {
    match (into.as_object_mut(), patch.as_object()) {
        (Some(target), Some(rows)) => {
            for (key, value) in rows {
                match target.get_mut(key) {
                    Some(slot) if slot.is_object() && value.is_object() => merge(slot, value),
                    _ => {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        _ => *into = patch.clone(),
    }
}

pub fn demo() -> Option<Demo> {
    if let Some(cmd) = std::env::var_os("TENON_DEMO_PLUGIN") {
        let cmd = PathBuf::from(cmd);
        if cmd.is_file() {
            return Some(Demo {
                name: TERM_NAME.to_string(),
                cmd: cmd.display().to_string(),
                args: vec![],
                env: vec![],
            });
        }
    }
    let repo = repo()?;
    let term = repo.join("plugins/term/target/release/tenon-term");
    if term.is_file() {
        return Some(Demo {
            name: TERM_NAME.to_string(),
            cmd: term.display().to_string(),
            args: vec![],
            env: vec![],
        });
    }
    let guard = repo.join("playground/web/plugins/guard.py");
    let python = which("python3")?;
    if guard.is_file() {
        return Some(Demo {
            name: GUARD_NAME.to_string(),
            cmd: python,
            args: vec![guard.display().to_string()],
            env: vec![(
                "PYTHONPATH".to_string(),
                repo.join("sdk/py").display().to_string(),
            )],
        });
    }
    None
}

fn repo() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TENON_REPO") {
        return Some(PathBuf::from(path));
    }
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("kernel/src/tenon.erl").is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.display().to_string())
}

fn entry_list(demo: Option<&Demo>) -> String {
    match demo {
        None => "[]\n".to_string(),
        Some(demo) => format!("- id: {DEMO_ID}\n  name: {}\n", demo.name),
    }
}

fn registry_map(demo: Option<&Demo>) -> String {
    match demo {
        None => "{}\n".to_string(),
        Some(demo) => format!(
            "\"{}\":\n  cmd: {}\n  args: {}\n  env: {}\n",
            demo.name,
            demo.cmd,
            yaml_list(demo.args.iter().map(|arg| arg.to_string())),
            yaml_list(
                demo.env
                    .iter()
                    .map(|(name, value)| format!("[{name}, {value}]"))
            )
        ),
    }
}

fn yaml_list(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    format!("[{}]", items.join(", "))
}

fn copy_file(from: &Path, into: &Path) -> Result<()> {
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

pub fn copy_tree(from: &Path, into: &Path) -> Result<()> {
    if !from.is_dir() {
        bail!("{} is not a directory", from.display());
    }
    std::fs::create_dir_all(into)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = into.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            copy_file(&entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_distinguishes_homes() {
        let a = Home {
            root: PathBuf::from("/tmp/tenon-home-a"),
        };
        let again = Home {
            root: PathBuf::from("/tmp/tenon-home-a"),
        };
        let b = Home {
            root: PathBuf::from("/tmp/tenon-home-b"),
        };
        assert_eq!(a.hash(), again.hash());
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash().len(), 12);
        assert!(a.hash().chars().all(|c| c.is_ascii_hexdigit()));
    }
}
