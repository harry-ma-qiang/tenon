use crate::{now, Store};
use anyhow::Result;
use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;

/// One proposal of RFC section 10's change protocol, from `propose` to its
/// terminal `promoted` or `rolled_back`. The row is the truth: base keeps no
/// second copy of it in memory, so `upgrade.status` after a restart still
/// answers what happened.
#[derive(Debug, Clone, Serialize)]
pub struct Upgrade {
    pub id: i64,
    pub env: String,
    pub target: String,
    pub status: String,
    pub artifact: String,
    pub notes: String,
    pub reason: Option<String>,
    pub phases: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// One benchmark pass: the promotion gate's measurement, recorded for the LKG
/// and for every candidate so the two can be compared by the same numbers.
#[derive(Debug, Clone, Serialize)]
pub struct Benchmark {
    pub id: i64,
    pub env: String,
    pub label: String,
    pub upgrade_id: Option<i64>,
    pub tasks: i64,
    pub passed: i64,
    pub success_rate: f64,
    pub cost: i64,
    pub lkg: i64,
    pub created_at: i64,
}

fn upgrade(row: &Row<'_>) -> rusqlite::Result<Upgrade> {
    Ok(Upgrade {
        id: row.get(0)?,
        env: row.get(1)?,
        target: row.get(2)?,
        status: row.get(3)?,
        artifact: row.get(4)?,
        notes: row.get(5)?,
        reason: row.get(6)?,
        phases: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn benchmark(row: &Row<'_>) -> rusqlite::Result<Benchmark> {
    Ok(Benchmark {
        id: row.get(0)?,
        env: row.get(1)?,
        label: row.get(2)?,
        upgrade_id: row.get(3)?,
        tasks: row.get(4)?,
        passed: row.get(5)?,
        success_rate: row.get(6)?,
        cost: row.get(7)?,
        lkg: row.get(8)?,
        created_at: row.get(9)?,
    })
}

const COLUMNS: &str =
    "id, env, target, status, artifact, notes, reason, phases, created_at, updated_at";
const BENCH_COLUMNS: &str =
    "id, env, label, upgrade_id, tasks, passed, success_rate, cost, lkg, created_at";

impl Store {
    pub fn put_upgrade(
        &self,
        env: &str,
        target: &str,
        artifact: &str,
        notes: &str,
        status: &str,
    ) -> Result<i64> {
        let at = now();
        self.conn.execute(
            "insert into upgrades (env, target, status, artifact, notes, phases, created_at, \
             updated_at) values (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?6)",
            params![env, target, status, artifact, notes, at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_upgrade(
        &self,
        id: i64,
        status: &str,
        reason: Option<&str>,
        phases: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "update upgrades set status = ?2, reason = coalesce(?3, reason), phases = ?4, \
             updated_at = ?5 where id = ?1",
            params![id, status, reason, phases, now()],
        )?;
        Ok(changed > 0)
    }

    pub fn upgrade(&self, id: i64) -> Result<Option<Upgrade>> {
        let sql = format!("select {COLUMNS} from upgrades where id = ?1");
        Ok(self.conn.query_row(&sql, params![id], upgrade).optional()?)
    }

    pub fn upgrades(&self, env: Option<&str>, limit: i64) -> Result<Vec<Upgrade>> {
        let sql = format!(
            "select {COLUMNS} from upgrades where (?1 is null or env = ?1) \
             order by id desc limit ?2"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![env, limit], upgrade)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// A new LKG row supersedes the old one: the baseline is what the current
    /// good runtime scores now, not what some earlier version scored once.
    pub fn put_benchmark(
        &self,
        env: &str,
        label: &str,
        upgrade_id: Option<i64>,
        row: (i64, i64, f64, i64),
        lkg: bool,
    ) -> Result<i64> {
        let (tasks, passed, success_rate, cost) = row;
        if lkg {
            self.conn.execute(
                "update benchmarks set lkg = 0 where env = ?1 and label = ?2",
                params![env, label],
            )?;
        }
        self.conn.execute(
            "insert into benchmarks (env, label, upgrade_id, tasks, passed, success_rate, cost, \
             lkg, created_at) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                env,
                label,
                upgrade_id,
                tasks,
                passed,
                success_rate,
                cost,
                i64::from(lkg),
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn lkg_benchmark(&self, env: &str, label: &str) -> Result<Option<Benchmark>> {
        let sql = format!(
            "select {BENCH_COLUMNS} from benchmarks where env = ?1 and label = ?2 and lkg = 1 \
             order by id desc limit 1"
        );
        Ok(self
            .conn
            .query_row(&sql, params![env, label], benchmark)
            .optional()?)
    }

    pub fn benchmarks(&self, env: Option<&str>, limit: i64) -> Result<Vec<Benchmark>> {
        let sql = format!(
            "select {BENCH_COLUMNS} from benchmarks where (?1 is null or env = ?1) \
             order by id desc limit ?2"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params![env, limit], benchmark)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }
}
