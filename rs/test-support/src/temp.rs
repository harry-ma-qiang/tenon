use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A throwaway directory under the system temp dir, removed on drop. Every
/// name carries the pid and a counter, so parallel test threads never share
/// one.
pub struct Temp {
    path: PathBuf,
}

impl Temp {
    pub fn new(tag: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("tenon-{tag}-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.path.join(rel)
    }

    pub fn put(&self, rel: &str, text: &str) {
        let target = self.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).expect("temp parent");
        }
        std::fs::write(target, text).expect("temp write");
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.join(rel)).expect("temp read")
    }

    pub fn has(&self, rel: &str) -> bool {
        self.join(rel).exists()
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
