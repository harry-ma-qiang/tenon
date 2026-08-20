use crate::{now, Store};
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

/// One row per step of the loop, written from day one so the navigator (P6)
/// has a table before it exists: what the world looked like (`state_hash`),
/// what the agent did (`action`), how well it went (`verifier_score`) and what
/// it cost (`cost`, the step's token usage).
#[derive(Debug, Clone, Serialize)]
pub struct Episode {
    pub id: i64,
    pub session_id: String,
    pub step: i64,
    pub state_hash: String,
    pub action: Value,
    pub verifier_score: Option<f64>,
    pub cost: Value,
    pub created_at: i64,
}

impl Store {
    pub fn put_episode(
        &self,
        session_id: &str,
        step: i64,
        state_hash: &str,
        action: &Value,
        verifier_score: Option<f64>,
        cost: &Value,
    ) -> Result<i64> {
        self.conn.execute(
            "insert into episodes
               (session_id, step, state_hash, action, verifier_score, cost, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                step,
                state_hash,
                action.to_string(),
                verifier_score,
                cost.to_string(),
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn episodes_of_session(&self, session_id: &str, limit: i64) -> Result<Vec<Episode>> {
        let mut stmt = self.conn.prepare(
            "select id, session_id, step, state_hash, action, verifier_score, cost, created_at
             from episodes where session_id = ?1 order by id limit ?2",
        )?;
        let rows = stmt.query_map(params![session_id, limit.max(1)], episode)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn episode_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from episodes", [], |row| row.get(0))?)
    }
}

fn episode(row: &rusqlite::Row<'_>) -> rusqlite::Result<Episode> {
    let action: String = row.get(4)?;
    let cost: String = row.get(6)?;
    Ok(Episode {
        id: row.get(0)?,
        session_id: row.get(1)?,
        step: row.get(2)?,
        state_hash: row.get(3)?,
        action: serde_json::from_str(&action).unwrap_or(Value::Null),
        verifier_score: row.get(5)?,
        cost: serde_json::from_str(&cost).unwrap_or(Value::Null),
        created_at: row.get(7)?,
    })
}
