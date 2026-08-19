pub mod fs;
pub mod pty;
pub mod service;
pub mod snap;

pub use tenon_sdk::{Error, Result};

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const TAIL_BYTES: usize = 32 * 1024;
pub const OUT_DIR: &str = ".tenon-out";
pub const SNAP_DIR: &str = ".tenon-snap";
pub const RING_BYTES: usize = 256 * 1024;
pub const DEFAULT_SERVICE: &str = "worker";

pub fn err(message: impl Into<String>) -> Error {
    Error::msg(message.into())
}

pub fn workspace(args: &[String]) -> PathBuf {
    let mut items = args.iter();
    while let Some(arg) = items.next() {
        if arg == "--workspace" {
            if let Some(dir) = items.next() {
                return PathBuf::from(dir);
            }
        }
        if let Some(dir) = arg.strip_prefix("--workspace=") {
            return PathBuf::from(dir);
        }
    }
    if let Some(dir) = std::env::var_os("TENON_WORKSPACE") {
        return PathBuf::from(dir);
    }
    if Path::new("/workspace").is_dir() {
        return PathBuf::from("/workspace");
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn out_dir(root: &Path) -> Result<PathBuf> {
    let dir = root.join(OUT_DIR);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn run(args: &[String]) -> i32 {
    let root = workspace(args);
    if let Err(error) = std::fs::create_dir_all(&root) {
        eprintln!("tenon worker: workspace {}: {error}", root.display());
        return 2;
    }
    match service::serve(&root) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("tenon worker: {error}");
            2
        }
    }
}

pub fn ok(extra: Value) -> Value {
    let mut base = json!({"ok": true});
    if let (Some(target), Some(rows)) = (base.as_object_mut(), extra.as_object()) {
        for (key, value) in rows {
            target.insert(key.clone(), value.clone());
        }
    }
    base
}
