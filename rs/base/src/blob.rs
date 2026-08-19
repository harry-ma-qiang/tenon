use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tenon_storage::Store;

/// RFC section 3's blob facade: content-addressed, deduplicated bytes over the
/// existing `blobs` table. The sha256 hash is the only handle, and possession
/// of a 256-bit hash is the read capability — blobs are shared and dedup'd
/// across envs by construction (per-env spill files are deferred, RFC section
/// 6), so scoping is capability-by-hash rather than a per-env partition.
pub struct BlobFacade {
    store: Arc<Mutex<Store>>,
}

impl BlobFacade {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }

    pub fn put(&self, bytes: &[u8]) -> Result<Value, String> {
        let hash = self
            .store
            .lock()
            .expect("blob store")
            .put_blob(bytes)
            .map_err(|error| error.to_string())?;
        Ok(json!({"hash": hash, "size": bytes.len()}))
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>, String> {
        let store = self.store.lock().expect("blob store");
        match store.get_blob(hash).map_err(|error| error.to_string())? {
            Some(bytes) => Ok(bytes),
            None => Err(format!("unknown blob {hash}")),
        }
    }

    /// The incremental window `{offset, len}` names — the paged read that keeps a
    /// 100 MB payload from being materialised whole.
    pub fn open(&self, hash: &str, offset: i64, len: i64) -> Result<Vec<u8>, String> {
        self.store
            .lock()
            .expect("blob store")
            .open_blob(hash, offset, len)
            .map_err(|error| error.to_string())
    }

    pub fn stat(&self, hash: &str) -> Result<Value, String> {
        let store = self.store.lock().expect("blob store");
        match store.blob(hash).map_err(|error| error.to_string())? {
            Some(row) => Ok(json!({
                "hash": row.sha256,
                "size": row.size,
                "created_at": row.created_at,
            })),
            None => Err(format!("unknown blob {hash}")),
        }
    }
}
