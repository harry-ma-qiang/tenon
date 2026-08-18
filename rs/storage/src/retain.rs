use crate::{now, Store};
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

/// RFC section 8's growth control as one struct: keep the newest `keep_steps`
/// workspace snapshots, one milestone every `milestone_every` steps, and
/// anything an LKG points at, forever. `keep_events` is opt-in and 0 by
/// default, because the event log is the version history — a host that wants
/// a bounded file rather than a complete one sets it.
#[derive(Debug, Clone)]
pub struct Retention {
    pub keep_steps: i64,
    pub milestone_every: i64,
    pub keep_refs: Vec<String>,
    pub keep_events: i64,
    pub blob_grace_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Retained {
    pub packs: usize,
    pub snapshots: usize,
    pub events: usize,
    pub tool_results: usize,
    pub blobs: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_steps: 40,
            milestone_every: 10,
            keep_refs: Vec::new(),
            keep_events: 0,
            blob_grace_ms: 60_000,
        }
    }
}

impl Retention {
    pub fn keeps(&self, step: i64, newest: i64, reference: &str) -> bool {
        if step > newest - self.keep_steps.max(0) {
            return true;
        }
        if self.milestone_every > 0 && step % self.milestone_every == 0 {
            return true;
        }
        self.keep_refs.iter().any(|kept| kept == reference)
    }
}

impl Store {
    /// Forward-only pruning in one pass: packs and their snapshot rows first,
    /// then the event window if one is configured, then the tool results whose
    /// event is gone, then every blob nothing references any more.
    pub fn retain(&self, policy: &Retention) -> Result<Retained> {
        let mut out = Retained::default();
        let index = self.pack_index()?;
        let newest = index.last().map(|(step, _)| *step).unwrap_or(0);
        let doomed: Vec<i64> = index
            .iter()
            .filter(|(step, reference)| !policy.keeps(*step, newest, reference))
            .map(|(step, _)| *step)
            .collect();
        if !doomed.is_empty() {
            let list = doomed
                .iter()
                .map(|step| step.to_string())
                .collect::<Vec<String>>()
                .join(",");
            out.packs = self
                .conn
                .execute(&format!("delete from packs where step in ({list})"), [])?;
            out.snapshots = self
                .conn
                .execute(&format!("delete from snapshots where step in ({list})"), [])?;
        }
        if policy.keep_events > 0 {
            let last = self.last_event_id()?;
            out.events = self.conn.execute(
                "delete from events where id <= ?1",
                params![last - policy.keep_events],
            )?;
            out.tool_results = self.conn.execute(
                "delete from tool_results where event_id not in (select id from events)",
                [],
            )?;
        }
        out.blobs = self.conn.execute(
            "delete from blobs where created_at <= ?1
             and sha256 not in (select blob_hash from tool_results where blob_hash is not null)",
            params![now() - policy.blob_grace_ms.max(0)],
        )?;
        self.incremental_vacuum(0)?;
        Ok(out)
    }
}
