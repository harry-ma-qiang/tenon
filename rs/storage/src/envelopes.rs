use crate::Store;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

/// One durable bus envelope as stored: the closed-core columns the query and
/// scope paths index on, plus the whole envelope as a JSON `body`. `seq` is the
/// monotonic log offset a `since_offset` subscribe replays from.
pub struct EnvelopeRow<'a> {
    pub event_id: &'a str,
    pub topic: &'a str,
    pub env: Option<&'a str>,
    pub ts: i64,
    pub body: &'a str,
}

impl Store {
    /// Persist a batch in one transaction (the hub's group commit). Duplicate
    /// `event_id`s are ignored — at-least-once delivery, effectively-once in the
    /// store — and the existing offset is returned for the duplicate.
    pub fn append_envelopes(&self, rows: &[EnvelopeRow<'_>]) -> Result<Vec<u64>> {
        let tx = self.conn.unchecked_transaction()?;
        let mut offsets = Vec::with_capacity(rows.len());
        {
            let mut insert = tx.prepare_cached(
                "insert or ignore into envelopes (event_id, topic, env, ts, body)
                 values (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut lookup = tx.prepare_cached("select seq from envelopes where event_id = ?1")?;
            for row in rows {
                insert.execute(params![row.event_id, row.topic, row.env, row.ts, row.body])?;
                let seq: i64 = if tx.changes() > 0 {
                    tx.last_insert_rowid()
                } else {
                    lookup.query_row(params![row.event_id], |r| r.get(0))?
                };
                offsets.push(seq.max(0) as u64);
            }
        }
        tx.commit()?;
        Ok(offsets)
    }

    /// Replay: every envelope body after `after`, oldest first, optionally
    /// scoped to one env (RFC 8d.2).
    pub fn envelopes_since(
        &self,
        after: i64,
        env: Option<&str>,
        limit: i64,
    ) -> Result<Vec<(u64, String)>> {
        let mut stmt = self.conn.prepare(
            "select seq, body from envelopes
             where seq > ?1 and (?2 is null or env = ?2)
             order by seq limit ?3",
        )?;
        let rows = stmt.query_map(params![after, env, limit.max(0)], |row| {
            let seq: i64 = row.get(0)?;
            let body: String = row.get(1)?;
            Ok((seq.max(0) as u64, body))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn envelopes_head(&self) -> Result<u64> {
        let seq: Option<i64> = self
            .conn
            .query_row("select max(seq) from envelopes", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(seq.unwrap_or(0).max(0) as u64)
    }

    pub fn envelope_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("select count(*) from envelopes", [], |row| row.get(0))?)
    }
}
