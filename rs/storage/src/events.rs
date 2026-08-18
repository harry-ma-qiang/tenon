use crate::{now, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub env: Option<String>,
    pub data: Value,
}

/// One row per tool call the loop dispatched. `event_id` is the `tool/result`
/// row in `events`, so the log stays the truth and this table is the index
/// over it; `blob_hash` is set when the full output went to `blobs` because it
/// was too large to keep in the event.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    pub id: i64,
    pub event_id: i64,
    pub name: String,
    pub status: String,
    pub duration_ms: i64,
    pub blob_hash: Option<String>,
    pub created_at: i64,
}

impl Store {
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

    pub fn event_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from events", [], |row| row.get(0))?)
    }

    pub fn put_tool_result(
        &self,
        event_id: i64,
        name: &str,
        status: &str,
        duration_ms: i64,
        blob_hash: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "insert into tool_results (event_id, name, status, duration_ms, blob_hash, created_at)
             values (?1, ?2, ?3, ?4, ?5, ?6)",
            params![event_id, name, status, duration_ms, blob_hash, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn tool_results_tail(&self, limit: i64) -> Result<Vec<ToolResult>> {
        let mut stmt = self.conn.prepare(
            "select id, event_id, name, status, duration_ms, blob_hash, created_at
             from tool_results order by id desc limit ?1",
        )?;
        let rows = stmt.query_map(params![limit.max(1)], tool_result)?;
        let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn tool_results_of_event(&self, event_id: i64) -> Result<Vec<ToolResult>> {
        let mut stmt = self.conn.prepare(
            "select id, event_id, name, status, duration_ms, blob_hash, created_at
             from tool_results where event_id = ?1 order by id",
        )?;
        let rows = stmt.query_map(params![event_id], tool_result)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn tool_result_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from tool_results", [], |row| row.get(0))?)
    }
}

fn tool_result(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolResult> {
    Ok(ToolResult {
        id: row.get(0)?,
        event_id: row.get(1)?,
        name: row.get(2)?,
        status: row.get(3)?,
        duration_ms: row.get(4)?,
        blob_hash: row.get(5)?,
        created_at: row.get(6)?,
    })
}
