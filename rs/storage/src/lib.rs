pub mod approvals;
pub mod blobs;
pub mod envelopes;
pub mod episodes;
pub mod events;
pub mod kv;
pub mod memory;
pub mod packs;
pub mod query;
pub mod retain;
pub mod schema;
pub mod upgrades;

pub use approvals::Approval;
pub use blobs::{sha256, Blob};
pub use envelopes::EnvelopeRow;
pub use episodes::Episode;
pub use events::{Event, ToolResult};
pub use kv::KvRow;
pub use memory::{Embedding, MemoryEdge, MemoryNode};
pub use packs::{EnvRow, PackRow, SnapshotRow};
pub use query::{Aggregate, QueryFilter, Source, QUERY_INDEX_VERSION};
pub use retain::{Retained, Retention};
pub use schema::VERSION;
pub use upgrades::{Benchmark, Upgrade};

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// One host state file, one writer. Every table of RFC section 9 lives here;
/// readers (G, the CLI) open their own read-only connection to the same file.
pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open state file {}", path.display()))?;
        // Order matters: `auto_vacuum` only takes on a database with no tables
        // yet, so it is set before the schema is applied. An older file keeps
        // whatever it was created with and `incremental_vacuum` is a no-op
        // there until someone runs a full `vacuum`.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn version(&self) -> Result<i64> {
        schema::version(&self.conn)
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
    }

    /// Gives freed pages back to the filesystem in bounded chunks; `0` means
    /// everything on the free list. Cheap enough to run after every retention
    /// pass and never rewrites the whole file the way `vacuum` does.
    pub fn incremental_vacuum(&self, pages: i64) -> Result<()> {
        self.conn
            .pragma_update(None, "incremental_vacuum", pages.max(0))?;
        Ok(())
    }

    pub fn page_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "page_count", |row| row.get(0))?)
    }

    pub fn pragma(&self, name: &str) -> Result<String> {
        let value = self
            .conn
            .pragma_query_value(None, name, |row| row.get::<_, rusqlite::types::Value>(0))?;
        Ok(match value {
            rusqlite::types::Value::Text(text) => text,
            rusqlite::types::Value::Integer(number) => number.to_string(),
            other => format!("{other:?}"),
        })
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
