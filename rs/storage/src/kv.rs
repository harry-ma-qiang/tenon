use crate::Store;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

/// One durable kv row: an env-scoped key, its value, the global revision it was
/// last written at, and the optional TTL/lease that expires it. Ephemeral keys
/// never reach this table — they live in memory in the facade.
#[derive(Debug, Clone, Serialize)]
pub struct KvRow {
    pub env: String,
    pub key: String,
    #[serde(skip)]
    pub value: Vec<u8>,
    pub rev: i64,
    pub expires_at: Option<i64>,
    pub lease_id: Option<String>,
}

fn kv_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<KvRow> {
    Ok(KvRow {
        env: row.get(0)?,
        key: row.get(1)?,
        value: row.get(2)?,
        rev: row.get(3)?,
        expires_at: row.get(4)?,
        lease_id: row.get(5)?,
    })
}

const COLUMNS: &str = "env, key, value, rev, expires_at, lease_id";

impl Store {
    pub fn kv_get(&self, env: &str, key: &str) -> Result<Option<KvRow>> {
        let sql = format!("select {COLUMNS} from kv where env = ?1 and key = ?2");
        Ok(self
            .conn
            .query_row(&sql, params![env, key], kv_row)
            .optional()?)
    }

    pub fn kv_set(
        &self,
        env: &str,
        key: &str,
        value: &[u8],
        rev: i64,
        expires_at: Option<i64>,
        lease_id: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "insert into kv (env, key, value, rev, expires_at, lease_id)
             values (?1, ?2, ?3, ?4, ?5, ?6)
             on conflict(env, key) do update set
               value = excluded.value, rev = excluded.rev,
               expires_at = excluded.expires_at, lease_id = excluded.lease_id",
            params![env, key, value, rev, expires_at, lease_id],
        )?;
        Ok(())
    }

    pub fn kv_del(&self, env: &str, key: &str) -> Result<bool> {
        let gone = self.conn.execute(
            "delete from kv where env = ?1 and key = ?2",
            params![env, key],
        )?;
        Ok(gone > 0)
    }

    /// Every durable key of one env under `prefix`, keys ascending.
    pub fn kv_range(&self, env: &str, prefix: &str) -> Result<Vec<KvRow>> {
        let pattern = format!("{}%", escape_like(prefix));
        let sql = format!(
            "select {COLUMNS} from kv where env = ?1 and key like ?2 escape '\\'
             order by key"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![env, pattern], kv_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Keys whose TTL has passed, as `(env, key, lease_id)`: what the facade
    /// deletes and fires watch events for on the expiry tick.
    pub fn kv_expired(&self, now: i64) -> Result<Vec<(String, String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "select env, key, lease_id from kv where expires_at is not null and expires_at <= ?1",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every key bound to a lease: what expiry of that lease deletes.
    pub fn kv_by_lease(&self, lease_id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("select env, key from kv where lease_id = ?1")?;
        let rows = stmt.query_map(params![lease_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every durable key under `prefix` across all envs, keys ascending: what
    /// the timer wheel scans on boot to reload persisted timers (RFC 8d.2 keeps
    /// this base-internal — no scoped caller reaches it).
    pub fn kv_scan(&self, prefix: &str) -> Result<Vec<KvRow>> {
        let pattern = format!("{}%", escape_like(prefix));
        let sql =
            format!("select {COLUMNS} from kv where key like ?1 escape '\\' order by env, key");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![pattern], kv_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn kv_max_rev(&self) -> Result<i64> {
        let rev: Option<i64> = self
            .conn
            .query_row("select max(rev) from kv", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(rev.unwrap_or(0))
    }
}

/// `%` and `_` are `LIKE` wildcards; a prefix that contains one must match it
/// literally, so it is escaped with a backslash the query names as the escape.
fn escape_like(prefix: &str) -> String {
    prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
