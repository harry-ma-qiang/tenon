use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: &str = "
create table if not exists events (
  id integer primary key autoincrement,
  at integer not null,
  kind text not null,
  env text,
  data text not null
);
create table if not exists envs (
  name text primary key,
  role text not null,
  pid integer,
  status text not null,
  at integer not null,
  parent text,
  depth integer not null default 0
);
create table if not exists packs (
  step integer primary key,
  ref text not null,
  bytes blob not null,
  created_at integer not null
);
create index if not exists events_kind on events (kind);
";

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub env: Option<String>,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvRow {
    pub name: String,
    pub role: String,
    pub pid: Option<i64>,
    pub status: String,
    pub at: i64,
    pub parent: Option<String>,
    pub depth: i64,
}

#[derive(Debug, Clone)]
pub struct PackRow {
    pub step: i64,
    pub reference: String,
    pub bytes: Vec<u8>,
    pub created_at: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open state file {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn);
        Ok(Self { conn })
    }

    pub fn append(&self, kind: &str, env: Option<&str>, data: &Value) -> Result<Event> {
        let at = now();
        let body = data.to_string();
        self.conn.execute(
            "insert into events (at, kind, env, data) values (?1, ?2, ?3, ?4)",
            params![at, kind, env, body],
        )?;
        Ok(Event {
            id: self.conn.last_insert_rowid(),
            at,
            kind: kind.to_string(),
            env: env.map(str::to_string),
            data: data.clone(),
        })
    }

    pub fn events_since(&self, after: i64, limit: i64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "select id, at, kind, env, data from events where id > ?1 order by id limit ?2",
        )?;
        let rows = stmt.query_map(params![after, limit], |row| {
            let body: String = row.get(4)?;
            Ok(Event {
                id: row.get(0)?,
                at: row.get(1)?,
                kind: row.get(2)?,
                env: row.get(3)?,
                data: serde_json::from_str(&body).unwrap_or(Value::Null),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn last_event_id(&self) -> Result<i64> {
        let id: Option<i64> = self
            .conn
            .query_row("select max(id) from events", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(id.unwrap_or(0))
    }

    pub fn put_env(&self, name: &str, role: &str, pid: Option<i64>, status: &str) -> Result<()> {
        self.conn.execute(
            "insert into envs (name, role, pid, status, at) values (?1, ?2, ?3, ?4, ?5)
             on conflict(name) do update set role = ?2, pid = ?3, status = ?4, at = ?5",
            params![name, role, pid, status, now()],
        )?;
        Ok(())
    }

    pub fn put_env_parent(&self, name: &str, parent: Option<&str>, depth: i64) -> Result<()> {
        self.conn.execute(
            "update envs set parent = ?2, depth = ?3 where name = ?1",
            params![name, parent, depth],
        )?;
        Ok(())
    }

    pub fn drop_env(&self, name: &str) -> Result<()> {
        self.conn
            .execute("delete from envs where name = ?1", params![name])?;
        Ok(())
    }

    pub fn envs(&self) -> Result<Vec<EnvRow>> {
        let mut stmt = self
            .conn
            .prepare("select name, role, pid, status, at, parent, depth from envs order by name")?;
        let rows = stmt.query_map([], |row| {
            Ok(EnvRow {
                name: row.get(0)?,
                role: row.get(1)?,
                pid: row.get(2)?,
                status: row.get(3)?,
                at: row.get(4)?,
                parent: row.get(5)?,
                depth: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One row per snapshot pack pulled off that env's worker. The step is the
    /// worker's own snapshot step, so a re-pull of the same step is an upsert
    /// rather than a duplicate and `last_pack_step` is what the next pull asks
    /// the worker to skip.
    pub fn put_pack(&self, step: i64, reference: &str, bytes: &[u8]) -> Result<()> {
        self.conn.execute(
            "insert into packs (step, ref, bytes, created_at) values (?1, ?2, ?3, ?4)
             on conflict(step) do update set ref = ?2, bytes = ?3, created_at = ?4",
            params![step, reference, bytes, now()],
        )?;
        Ok(())
    }

    pub fn packs(&self) -> Result<Vec<PackRow>> {
        let mut stmt = self
            .conn
            .prepare("select step, ref, bytes, created_at from packs order by step")?;
        let rows = stmt.query_map([], |row| {
            Ok(PackRow {
                step: row.get(0)?,
                reference: row.get(1)?,
                bytes: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn last_pack_step(&self) -> Result<i64> {
        let step: Option<i64> = self
            .conn
            .query_row("select max(step) from packs", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(step.unwrap_or(0))
    }

    pub fn pack_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from packs", [], |row| row.get(0))?)
    }

    pub fn prune_packs(&self, keep_last: i64) -> Result<usize> {
        let removed = self.conn.execute(
            "delete from packs where step not in
               (select step from packs order by step desc limit ?1)",
            params![keep_last.max(1)],
        )?;
        Ok(removed)
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
    }
}

/// Columns added after the first release: an existing `state.sqlite` predates
/// them, and `create table if not exists` never adds a column to a table that
/// is already there. A duplicate-column error means the migration already ran.
fn migrate(conn: &Connection) {
    for statement in [
        "alter table envs add column parent text",
        "alter table envs add column depth integer not null default 0",
    ] {
        let _ = conn.execute(statement, []);
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempdir::Dir, Store) {
        let dir = tempdir::Dir::make();
        let store = Store::open(&dir.path().join("state.sqlite")).unwrap();
        (dir, store)
    }

    #[test]
    fn appends_events_in_order() {
        let (_dir, store) = store();
        let first = store.append("boot", None, &json!({"n": 1})).unwrap();
        let second = store
            .append("node", Some("root"), &json!({"n": 2}))
            .unwrap();
        assert!(second.id > first.id);
        let events = store.events_since(0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].env.as_deref(), Some("root"));
        assert_eq!(events[1].data["n"], 2);
        assert_eq!(store.last_event_id().unwrap(), second.id);
        assert_eq!(store.events_since(first.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn upserts_env_rows() {
        let (_dir, store) = store();
        store.put_env("root", "agent", Some(7), "starting").unwrap();
        store.put_env("root", "agent", Some(9), "up").unwrap();
        let envs = store.envs().unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].pid, Some(9));
        assert_eq!(envs[0].status, "up");
    }

    #[test]
    fn stores_packs_by_step_and_prunes_the_oldest() {
        let (_dir, store) = store();
        assert_eq!(store.last_pack_step().unwrap(), 0);
        for step in 1..=5 {
            store
                .put_pack(step, &format!("ref{step}"), b"pack")
                .unwrap();
        }
        store.put_pack(5, "ref5b", b"packpack").unwrap();
        assert_eq!(store.last_pack_step().unwrap(), 5);
        assert_eq!(store.pack_count().unwrap(), 5);
        let packs = store.packs().unwrap();
        assert_eq!(packs[4].reference, "ref5b");
        assert_eq!(packs[4].bytes, b"packpack");
        assert_eq!(store.prune_packs(2).unwrap(), 3);
        let left = store.packs().unwrap();
        assert_eq!(left.len(), 2);
        assert_eq!(left[0].step, 4);
    }

    #[test]
    fn records_the_environment_tree() {
        let (_dir, store) = store();
        store.put_env("root", "agent", Some(1), "up").unwrap();
        store.put_env("root.1", "agent", Some(2), "up").unwrap();
        store.put_env_parent("root.1", Some("root"), 1).unwrap();
        let envs = store.envs().unwrap();
        assert_eq!(envs[1].parent.as_deref(), Some("root"));
        assert_eq!(envs[1].depth, 1);
        assert_eq!(envs[0].depth, 0);
        store.put_env("root.1", "agent", Some(3), "down").unwrap();
        assert_eq!(store.envs().unwrap()[1].parent.as_deref(), Some("root"));
        store.drop_env("root.1").unwrap();
        assert_eq!(store.envs().unwrap().len(), 1);
    }

    #[test]
    fn sets_the_day_one_pragmas() {
        let (_dir, store) = store();
        let mode: String = store
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        let timeout: i64 = store
            .conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(timeout, 5000);
        store.checkpoint().unwrap();
    }

    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        pub struct Dir(PathBuf);

        impl Dir {
            pub fn make() -> Self {
                let seq = SEQ.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("tenon-storage-{}-{seq}", std::process::id()));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
