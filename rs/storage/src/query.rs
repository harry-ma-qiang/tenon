use crate::Store;
use anyhow::Result;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, OptionalExtension};
use serde_json::{json, Value};

/// The derived FTS index is a disposable read model over the events log (RFC
/// section 5, DSH pattern): a version bump drops and rebuilds it from the log,
/// so its shape can change without a schema migration.
pub const QUERY_INDEX_VERSION: i64 = 1;

const FTS_SELECT: &str = "select id, trim(\
  coalesce(json_extract(data,'$.text'),'')||' '||\
  coalesce(json_extract(data,'$.message.content'),'')||' '||\
  coalesce(json_extract(data,'$.name'),'')||' '||\
  coalesce(json_extract(data,'$.arguments'),'')) as body \
  from events where id > ?1 order by id";

#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    pub session: Option<String>,
    pub kind: Option<String>,
    pub topics: Vec<String>,
    pub status: Option<String>,
    pub name: Option<String>,
    pub level: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Events,
    Episodes,
    ToolResults,
}

impl Source {
    pub fn parse(name: &str) -> Option<Source> {
        match name {
            "" | "events" => Some(Source::Events),
            "episodes" => Some(Source::Episodes),
            "tool_results" | "tools" => Some(Source::ToolResults),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Source::Events => "events",
            Source::Episodes => "episodes",
            Source::ToolResults => "tool_results",
        }
    }

    fn time_col(&self) -> &'static str {
        match self {
            Source::Events => "at",
            _ => "created_at",
        }
    }
}

/// A typed aggregate over one source. `op` is `count`, `sum` or `avg`; the
/// field and group-by names resolve against a per-source allowlist so no
/// caller-supplied text ever reaches the SQL.
#[derive(Debug, Clone)]
pub struct Aggregate {
    pub op: String,
    pub field: Option<String>,
    pub group_by: Option<String>,
}

fn field_expr(source: Source, name: &str) -> Option<&'static str> {
    match (source, name) {
        (Source::Events, "at") => Some("at"),
        (Source::Events, "kind") => Some("kind"),
        (Source::Events, "session") => Some("json_extract(data,'$.session')"),
        (Source::Episodes, "verifier_score") => Some("verifier_score"),
        (Source::Episodes, "cost") | (Source::Episodes, "cost.total") => {
            Some("json_extract(cost,'$.total')")
        }
        (Source::Episodes, "step") => Some("step"),
        (Source::Episodes, "session") | (Source::Episodes, "session_id") => Some("session_id"),
        (Source::ToolResults, "duration_ms") => Some("duration_ms"),
        (Source::ToolResults, "status") => Some("status"),
        (Source::ToolResults, "name") => Some("name"),
        _ => None,
    }
}

fn sql_to_json(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(number) => json!(number),
        SqlValue::Real(number) => json!(number),
        SqlValue::Text(text) => json!(text),
        SqlValue::Blob(bytes) => json!(String::from_utf8_lossy(&bytes)),
    }
}

/// Each whitespace term becomes a quoted FTS5 phrase, so a hyphen or other
/// query-syntax character in a keyword is matched literally rather than parsed
/// as an operator.
fn fts_match(query: &str) -> String {
    query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<String>>()
        .join(" ")
}

/// A reserved namespace segment a `session/<kind>` topic carries in front of
/// the event `kind`, so a `topics` filter can name either the topic or the raw
/// kind and still match.
fn strip_ns(topic: &str) -> &str {
    const RESERVED: &[&str] = &[
        "session", "internal", "base", "budget", "approval", "guardian", "upgrade", "worker",
    ];
    match topic.split_once('/') {
        Some((ns, rest)) if RESERVED.contains(&ns) => rest,
        _ => topic,
    }
}

impl Store {
    /// Builds the composite hot-window indexes and the FTS virtual table on
    /// first use, rebuilds them from the log on a version bump, then walks the
    /// events appended since the last call into the index (incremental, off the
    /// events table). Cheap to call before every query.
    pub fn query_ensure_index(&self) -> Result<()> {
        self.conn.execute_batch(
            "create table if not exists query_meta (k text primary key, v integer not null);
             create index if not exists events_kind_at on events (kind, at);
             create index if not exists events_session_id on events (json_extract(data,'$.session'), id);
             create index if not exists episodes_created on episodes (created_at);
             create index if not exists tool_results_created on tool_results (created_at);",
        )?;
        if self.query_meta("index_version") != QUERY_INDEX_VERSION {
            self.query_rebuild_index()?;
        }
        self.query_catchup()?;
        Ok(())
    }

    fn query_meta(&self, key: &str) -> i64 {
        self.conn
            .query_row(
                "select v from query_meta where k = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    fn set_query_meta(&self, key: &str, value: i64) -> Result<()> {
        self.conn.execute(
            "insert or replace into query_meta (k, v) values (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Forces the next `query_ensure_index` to drop and rebuild the derived FTS
    /// index from the log, the way a `QUERY_INDEX_VERSION` bump does.
    pub fn query_reset_index(&self) -> Result<()> {
        self.conn.execute_batch(
            "create table if not exists query_meta (k text primary key, v integer not null);",
        )?;
        self.set_query_meta("index_version", 0)
    }

    fn query_rebuild_index(&self) -> Result<()> {
        self.conn.execute_batch(
            "drop table if exists events_fts;
             create virtual table events_fts using fts5(body, tokenize='unicode61');",
        )?;
        self.set_query_meta("last_event_id", 0)?;
        self.set_query_meta("index_version", QUERY_INDEX_VERSION)?;
        Ok(())
    }

    fn query_catchup(&self) -> Result<()> {
        let last = self.query_meta("last_event_id");
        let rows: Vec<(i64, String)> = {
            let mut stmt = self.conn.prepare(FTS_SELECT)?;
            let mapped = stmt.query_map(params![last], |row| Ok((row.get(0)?, row.get(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if rows.is_empty() {
            return Ok(());
        }
        let mut newest = last;
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut insert =
                tx.prepare_cached("insert into events_fts (rowid, body) values (?1, ?2)")?;
            for (id, body) in &rows {
                insert.execute(params![id, body])?;
                newest = newest.max(*id);
            }
        }
        tx.commit()?;
        self.set_query_meta("last_event_id", newest)
    }

    /// FTS5 over the event log's text payload fields, ranked by bm25, with a
    /// highlighted snippet and the source event id as the ref.
    pub fn query_text(&self, query: &str, filter: &QueryFilter, topk: i64) -> Result<Vec<Value>> {
        self.query_ensure_index()?;
        let matcher = fts_match(query);
        if matcher.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from(
            "select f.rowid, e.at, e.kind, json_extract(e.data,'$.session'), \
             snippet(events_fts, 0, '[', ']', '...', 12), bm25(events_fts) \
             from events_fts f join events e on e.id = f.rowid \
             where events_fts match ?",
        );
        let mut binds: Vec<SqlValue> = vec![SqlValue::Text(matcher)];
        text_where(&mut sql, &mut binds, "e", filter);
        sql.push_str(" order by bm25(events_fts) limit ?");
        binds.push(SqlValue::Integer(topk.max(1)));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |row| {
            Ok(json!({
                "ref": row.get::<_, i64>(0)?,
                "at": row.get::<_, i64>(1)?,
                "kind": row.get::<_, String>(2)?,
                "session": row.get::<_, Option<String>>(3)?,
                "snippet": row.get::<_, String>(4)?,
                "rank": row.get::<_, f64>(5)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Typed numeric/aggregate scan over event columns and json-extracted
    /// fields. With no aggregate it returns the newest rows of the source; with
    /// one it groups and reduces. Parameterised throughout, no SQL exposed.
    pub fn query_scan(
        &self,
        source: Source,
        filter: &QueryFilter,
        aggregate: Option<Aggregate>,
        limit: i64,
    ) -> Result<Value> {
        let (where_sql, binds) = scan_where(source, filter);
        match aggregate {
            Some(aggregate) => self.scan_aggregate(source, &where_sql, binds, &aggregate),
            None => self.scan_rows(source, &where_sql, binds, limit),
        }
    }

    fn scan_rows(
        &self,
        source: Source,
        where_sql: &str,
        mut binds: Vec<SqlValue>,
        limit: i64,
    ) -> Result<Value> {
        let columns = match source {
            Source::Events => "id, at, kind, data",
            Source::Episodes => {
                "id, session_id, step, state_hash, action, verifier_score, cost, created_at"
            }
            Source::ToolResults => "id, event_id, name, status, duration_ms, blob_hash, created_at",
        };
        let sql = format!(
            "select {columns} from {} {where_sql} order by id desc limit ?",
            source.label()
        );
        binds.push(SqlValue::Integer(limit.max(1)));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |row| {
            Ok(scan_row(source, row))
        })?;
        let mut rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(json!({"count": rows.len(), "rows": rows}))
    }

    fn scan_aggregate(
        &self,
        source: Source,
        where_sql: &str,
        binds: Vec<SqlValue>,
        aggregate: &Aggregate,
    ) -> Result<Value> {
        let reducer = match aggregate.op.as_str() {
            "count" => "count(*)".to_string(),
            "sum" | "avg" => {
                let field = aggregate
                    .field
                    .as_deref()
                    .and_then(|name| field_expr(source, name))
                    .ok_or_else(|| anyhow::anyhow!("unknown_field"))?;
                format!("{}({field})", aggregate.op)
            }
            other => return Err(anyhow::anyhow!("unknown_aggregate:{other}")),
        };
        let group = match aggregate.group_by.as_deref() {
            Some(name) => {
                Some(field_expr(source, name).ok_or_else(|| anyhow::anyhow!("unknown_group_by"))?)
            }
            None => None,
        };
        let sql = match group {
            Some(expr) => format!(
                "select {reducer}, {expr} from {} {where_sql} group by {expr} order by 1 desc",
                source.label()
            ),
            None => format!("select {reducer}, null from {} {where_sql}", source.label()),
        };
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |row| {
            Ok(json!({
                "value": sql_to_json(row.get::<_, SqlValue>(0)?),
                "key": sql_to_json(row.get::<_, SqlValue>(1)?),
            }))
        })?;
        let groups = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"count": groups.len(), "groups": groups}))
    }
}

fn scan_row(source: Source, row: &rusqlite::Row<'_>) -> Value {
    match source {
        Source::Events => json!({
            "id": row.get::<_, i64>(0).unwrap_or(0),
            "at": row.get::<_, i64>(1).unwrap_or(0),
            "kind": row.get::<_, String>(2).unwrap_or_default(),
            "data": parse_json(row.get::<_, String>(3).unwrap_or_default()),
        }),
        Source::Episodes => json!({
            "id": row.get::<_, i64>(0).unwrap_or(0),
            "session_id": row.get::<_, String>(1).unwrap_or_default(),
            "step": row.get::<_, i64>(2).unwrap_or(0),
            "state_hash": row.get::<_, String>(3).unwrap_or_default(),
            "action": parse_json(row.get::<_, String>(4).unwrap_or_default()),
            "verifier_score": row.get::<_, Option<f64>>(5).unwrap_or(None),
            "cost": parse_json(row.get::<_, String>(6).unwrap_or_default()),
            "created_at": row.get::<_, i64>(7).unwrap_or(0),
        }),
        Source::ToolResults => json!({
            "id": row.get::<_, i64>(0).unwrap_or(0),
            "event_id": row.get::<_, i64>(1).unwrap_or(0),
            "name": row.get::<_, String>(2).unwrap_or_default(),
            "status": row.get::<_, String>(3).unwrap_or_default(),
            "duration_ms": row.get::<_, i64>(4).unwrap_or(0),
            "blob_hash": row.get::<_, Option<String>>(5).unwrap_or(None),
            "created_at": row.get::<_, i64>(6).unwrap_or(0),
        }),
    }
}

fn parse_json(text: String) -> Value {
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

fn text_where(sql: &mut String, binds: &mut Vec<SqlValue>, alias: &str, filter: &QueryFilter) {
    if let Some(session) = &filter.session {
        sql.push_str(&format!(" and json_extract({alias}.data,'$.session') = ?"));
        binds.push(SqlValue::Text(session.clone()));
    }
    let kinds = kind_candidates(filter);
    if !kinds.is_empty() {
        let holes = vec!["?"; kinds.len()].join(",");
        sql.push_str(&format!(" and {alias}.kind in ({holes})"));
        for kind in kinds {
            binds.push(SqlValue::Text(kind));
        }
    }
    if let Some(level) = &filter.level {
        sql.push_str(&format!(" and json_extract({alias}.data,'$.level') = ?"));
        binds.push(SqlValue::Text(level.clone()));
    }
    if let Some(since) = filter.since {
        sql.push_str(&format!(" and {alias}.at >= ?"));
        binds.push(SqlValue::Integer(since));
    }
    if let Some(until) = filter.until {
        sql.push_str(&format!(" and {alias}.at <= ?"));
        binds.push(SqlValue::Integer(until));
    }
}

fn kind_candidates(filter: &QueryFilter) -> Vec<String> {
    let mut kinds: Vec<String> = filter
        .topics
        .iter()
        .map(|topic| strip_ns(topic).to_string())
        .collect();
    if let Some(kind) = &filter.kind {
        kinds.push(kind.clone());
    }
    kinds
}

fn scan_where(source: Source, filter: &QueryFilter) -> (String, Vec<SqlValue>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut binds: Vec<SqlValue> = Vec::new();
    match source {
        Source::Events => {
            if let Some(session) = &filter.session {
                clauses.push("json_extract(data,'$.session') = ?".to_string());
                binds.push(SqlValue::Text(session.clone()));
            }
            let kinds = kind_candidates(filter);
            if !kinds.is_empty() {
                let holes = vec!["?"; kinds.len()].join(",");
                clauses.push(format!("kind in ({holes})"));
                for kind in kinds {
                    binds.push(SqlValue::Text(kind));
                }
            }
        }
        Source::Episodes => {
            if let Some(session) = &filter.session {
                clauses.push("session_id = ?".to_string());
                binds.push(SqlValue::Text(session.clone()));
            }
        }
        Source::ToolResults => {
            if let Some(status) = &filter.status {
                clauses.push("status = ?".to_string());
                binds.push(SqlValue::Text(status.clone()));
            }
            if let Some(name) = &filter.name {
                clauses.push("name = ?".to_string());
                binds.push(SqlValue::Text(name.clone()));
            }
        }
    }
    let time = source.time_col();
    if let Some(since) = filter.since {
        clauses.push(format!("{time} >= ?"));
        binds.push(SqlValue::Integer(since));
    }
    if let Some(until) = filter.until {
        clauses.push(format!("{time} <= ?"));
        binds.push(SqlValue::Integer(until));
    }
    let sql = match clauses.is_empty() {
        true => String::new(),
        false => format!("where {}", clauses.join(" and ")),
    };
    (sql, binds)
}
