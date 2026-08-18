use crate::{now, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

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

/// `snapshots` is `packs` without the payload: the step-to-ref index a reader
/// (the CLI, a navigator plugin, `episodes` looking for the workspace state a
/// step started from) can scan without pulling megabytes of packfile.
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRow {
    pub step: i64,
    pub reference: String,
    pub created_at: i64,
}

impl Store {
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
    /// the worker to skip. The matching `snapshots` row is written with it.
    pub fn put_pack(&self, step: i64, reference: &str, bytes: &[u8]) -> Result<()> {
        self.conn.execute(
            "insert into packs (step, ref, bytes, created_at) values (?1, ?2, ?3, ?4)
             on conflict(step) do update set ref = ?2, bytes = ?3, created_at = ?4",
            params![step, reference, bytes, now()],
        )?;
        self.put_snapshot(step, reference)
    }

    pub fn put_snapshot(&self, step: i64, reference: &str) -> Result<()> {
        self.conn.execute(
            "insert into snapshots (step, ref, created_at) values (?1, ?2, ?3)
             on conflict(step) do update set ref = ?2, created_at = ?3",
            params![step, reference, now()],
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

    pub fn snapshots(&self) -> Result<Vec<SnapshotRow>> {
        let mut stmt = self
            .conn
            .prepare("select step, ref, created_at from snapshots order by step")?;
        let rows = stmt.query_map([], |row| {
            Ok(SnapshotRow {
                step: row.get(0)?,
                reference: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn head_snapshot(&self) -> Result<Option<SnapshotRow>> {
        let row = self
            .conn
            .query_row(
                "select step, ref, created_at from snapshots order by step desc limit 1",
                [],
                |row| {
                    Ok(SnapshotRow {
                        step: row.get(0)?,
                        reference: row.get(1)?,
                        created_at: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn last_pack_step(&self) -> Result<i64> {
        let step: Option<i64> = self
            .conn
            .query_row("select max(step) from packs", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(step.unwrap_or(0))
    }

    /// Step and ref without the payload: what the retention pass reads, so a
    /// prune never loads a megabyte of packfile to decide it can be dropped.
    pub fn pack_index(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("select step, ref from packs order by step")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
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
}
