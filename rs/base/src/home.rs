use anyhow::{bail, Context, Result};
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

    pub fn envs_dir(&self) -> PathBuf {
        self.root.join("envs")
    }

    pub fn env_dir(&self, env: &str) -> PathBuf {
        self.envs_dir().join(env)
    }

    pub fn workspace_dir(&self, env: &str) -> PathBuf {
        self.env_dir(env).join("workspace")
    }

    pub fn gateway_sock(&self, env: &str) -> PathBuf {
        self.run().join(format!("gateway-{env}.sock"))
    }

    pub fn gateway_address(&self, env: &str) -> String {
        format!("unix:{}", self.gateway_sock(env).display())
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

    pub fn lock_file(&self) -> PathBuf {
        self.run().join("base.lock")
    }

    pub fn lkg_state_file(&self) -> PathBuf {
        self.lkg().join("state.sqlite")
    }

    pub fn lkg(&self) -> PathBuf {
        self.root.join("lkg")
    }

    pub fn scaffold(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.profiles(),
            self.erts(),
            self.run(),
            self.lkg(),
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

fn copy_tree(from: &Path, into: &Path) -> Result<()> {
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
