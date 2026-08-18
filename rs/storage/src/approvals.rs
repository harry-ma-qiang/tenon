use crate::{now, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

pub const PENDING: &str = "pending";
pub const APPROVED: &str = "approved";
pub const DENIED: &str = "denied";
pub const EXPIRED: &str = "expired";

/// The approval queue G owns from P3.5 on. P3.4 writes the row and its
/// immediate verdict, so the history of what was asked and answered is in the
/// state file rather than only in the event log.
#[derive(Debug, Clone, Serialize)]
pub struct Approval {
    pub id: i64,
    pub env: String,
    pub reason: String,
    pub status: String,
    pub created_at: i64,
    pub decided_at: Option<i64>,
}

impl Store {
    pub fn put_approval(&self, env: &str, reason: &str, status: &str) -> Result<i64> {
        let at = now();
        let decided = match status {
            PENDING => None,
            _ => Some(at),
        };
        self.conn.execute(
            "insert into approvals (env, reason, status, created_at, decided_at)
             values (?1, ?2, ?3, ?4, ?5)",
            params![env, reason, status, at, decided],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn decide_approval(&self, id: i64, status: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "update approvals set status = ?2, decided_at = ?3 where id = ?1 and status = ?4",
            params![id, status, now(), PENDING],
        )?;
        Ok(changed > 0)
    }

    pub fn approval(&self, id: i64) -> Result<Option<Approval>> {
        let row = self
            .conn
            .query_row(
                "select id, env, reason, status, created_at, decided_at from approvals
                 where id = ?1",
                params![id],
                approval,
            )
            .optional()?;
        Ok(row)
    }

    pub fn approvals(&self, status: Option<&str>, limit: i64) -> Result<Vec<Approval>> {
        let mut stmt = self.conn.prepare(
            "select id, env, reason, status, created_at, decided_at from approvals
             where (?1 is null or status = ?1) order by id desc limit ?2",
        )?;
        let rows = stmt.query_map(params![status, limit.max(1)], approval)?;
        let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// Everything still pending past its deadline becomes `expired`, which is
    /// the only state transition that happens without a human.
    pub fn expire_approvals(&self, older_than_ms: i64) -> Result<usize> {
        let cutoff = now() - older_than_ms.max(0);
        let changed = self.conn.execute(
            "update approvals set status = ?1, decided_at = ?2
             where status = ?3 and created_at <= ?4",
            params![EXPIRED, now(), PENDING, cutoff],
        )?;
        Ok(changed)
    }
}

fn approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<Approval> {
    Ok(Approval {
        id: row.get(0)?,
        env: row.get(1)?,
        reason: row.get(2)?,
        status: row.get(3)?,
        created_at: row.get(4)?,
        decided_at: row.get(5)?,
    })
}
