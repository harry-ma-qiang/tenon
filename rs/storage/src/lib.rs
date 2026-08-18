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
  at integer not null
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

    pub fn envs(&self) -> Result<Vec<EnvRow>> {
        let mut stmt = self
            .conn
            .prepare("select name, role, pid, status, at from envs order by name")?;
        let rows = stmt.query_map([], |row| {
            Ok(EnvRow {
                name: row.get(0)?,
                role: row.get(1)?,
                pid: row.get(2)?,
                status: row.get(3)?,
                at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
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
