use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

pub const VERSION: i64 = 5;

const VERSION_TABLE: &str = "
create table if not exists schema_version (
  version integer primary key,
  at integer not null
);
";

const V1: &str = "
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

const V2: &str = "
create table if not exists tool_results (
  id integer primary key autoincrement,
  event_id integer not null,
  name text not null,
  status text not null,
  duration_ms integer not null,
  blob_hash text,
  created_at integer not null
);
create index if not exists tool_results_event on tool_results (event_id);
create table if not exists snapshots (
  step integer primary key,
  ref text not null,
  created_at integer not null
);
create table if not exists blobs (
  sha256 text primary key,
  bytes blob not null,
  size integer not null,
  created_at integer not null
);
create table if not exists memory_nodes (
  id text primary key,
  kind text not null,
  text text not null,
  confidence real not null,
  outcomes text not null,
  created_at integer not null,
  updated_at integer not null
);
create table if not exists memory_edges (
  src text not null,
  dst text not null,
  rel text not null,
  confidence real not null,
  primary key (src, dst, rel)
);
create table if not exists embeddings (
  node_id text not null,
  model text not null,
  vector blob not null,
  dims integer not null,
  primary key (node_id, model)
);
create table if not exists episodes (
  id integer primary key autoincrement,
  session_id text not null,
  step integer not null,
  state_hash text not null,
  action text not null,
  verifier_score real,
  cost text not null,
  created_at integer not null
);
create index if not exists episodes_session on episodes (session_id);
create table if not exists approvals (
  id integer primary key autoincrement,
  env text not null,
  reason text not null,
  kind text not null default '',
  status text not null,
  note text,
  created_at integer not null,
  decided_at integer
);
";

const V3: &str = "
create table if not exists upgrades (
  id integer primary key autoincrement,
  env text not null,
  target text not null,
  status text not null,
  artifact text not null,
  notes text not null default '',
  reason text,
  phases text not null default '[]',
  created_at integer not null,
  updated_at integer not null
);
create index if not exists upgrades_env on upgrades (env);
create table if not exists benchmarks (
  id integer primary key autoincrement,
  env text not null,
  label text not null,
  upgrade_id integer,
  tasks integer not null,
  passed integer not null,
  success_rate real not null,
  cost integer not null,
  lkg integer not null default 0,
  created_at integer not null
);
create index if not exists benchmarks_env on benchmarks (env, label);
";

const V4: &str = "
create table if not exists envelopes (
  seq integer primary key autoincrement,
  event_id text not null unique,
  topic text not null,
  env text,
  ts integer not null,
  body text not null
);
create index if not exists envelopes_env on envelopes (env);
create index if not exists envelopes_topic on envelopes (topic);
create table if not exists kv (
  env text not null,
  key text not null,
  value blob not null,
  rev integer not null,
  expires_at integer,
  lease_id text,
  primary key (env, key)
);
create index if not exists kv_lease on kv (lease_id);
create index if not exists kv_expires on kv (expires_at);
";

const V5: &str = "
create table if not exists kv_meta (
  k text primary key,
  v integer not null
);
";

const STEPS: &[(i64, &str)] = &[(1, V1), (2, V2), (3, V3), (4, V4), (5, V5)];

/// Forward only: every state file carries the highest version it has been
/// migrated to, and a file written before `schema_version` existed reports 0
/// and is walked through every step. The steps are `create ... if not exists`
/// throughout, so replaying step 1 over a P3.2 file is a no-op that only
/// stamps the row.
pub fn migrate(conn: &Connection) -> Result<i64> {
    conn.execute_batch(VERSION_TABLE)
        .context("create schema_version")?;
    let current = version(conn)?;
    for (version, sql) in STEPS {
        if *version <= current {
            continue;
        }
        conn.execute_batch(sql)
            .with_context(|| format!("apply schema version {version}"))?;
        conn.execute(
            "insert or replace into schema_version (version, at) values (?1, ?2)",
            rusqlite::params![version, crate::now()],
        )?;
    }
    columns(conn);
    version(conn)
}

pub fn version(conn: &Connection) -> Result<i64> {
    let found: Option<i64> = conn
        .query_row("select max(version) from schema_version", [], |row| {
            row.get(0)
        })
        .optional()?
        .flatten();
    Ok(found.unwrap_or(0))
}

/// Columns added to a table that already existed: `create table if not exists`
/// never adds one, and a duplicate-column error means the migration already
/// ran. The `envs` pair arrived with P3.2, before this file had versions at
/// all; the `approvals` pair with P3.5, over files whose step 2 already ran.
fn columns(conn: &Connection) {
    for statement in [
        "alter table envs add column parent text",
        "alter table envs add column depth integer not null default 0",
        "alter table approvals add column kind text not null default ''",
        "alter table approvals add column note text",
    ] {
        let _ = conn.execute(statement, []);
    }
}
