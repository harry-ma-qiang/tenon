use crate::base::Base;
use crate::params::{i64_or, opt_text, strings, text, text_or};
use serde_json::{json, Value};
use tenon_storage::{Aggregate, QueryFilter, Source, Store};

type Answer = Result<Value, String>;

const TOPK: i64 = 20;
const TOPK_MAX: i64 = 500;
const SCAN_LIMIT: i64 = 200;
const SCAN_MAX: i64 = 20_000;

impl Base {
    /// The `query` facade (RFC section 5), served over the durable event log in
    /// the per-env state file. The env is already resolved through the single
    /// 8d.2 authorizer (`Conn::scoped_env`) before it reaches here, so a scoped
    /// caller can only ever land on its own env.
    pub fn query(&self, env: &str, method: &str, params: &Value) -> Answer {
        match method {
            "query.text" => self.query_text(env, params),
            "query.scan" => self.query_scan(env, params),
            "query.vector" => Ok(json!({
                "unsupported": true,
                "reason": "vector search is P5 (memory engine); P4 ships the query.vector interface only",
            })),
            other => Err(format!("unknown_method:{other}")),
        }
    }

    fn query_store(&self, env: &str) -> Result<&Store, String> {
        match env {
            "base" => Ok(&self.store),
            _ => self.store_of(env),
        }
    }

    fn query_text(&self, env: &str, params: &Value) -> Answer {
        let query = text(params, "q");
        let topk = i64_or(params, "topk", TOPK).clamp(1, TOPK_MAX);
        let filter = parse_filter(params);
        let hits = self
            .query_store(env)?
            .query_text(&query, &filter, topk)
            .map_err(|error| error.to_string())?;
        Ok(json!({"env": env, "q": query, "count": hits.len(), "hits": hits}))
    }

    fn query_scan(&self, env: &str, params: &Value) -> Answer {
        let source = Source::parse(&text_or(params, "source", "events"))
            .ok_or_else(|| "unknown source".to_string())?;
        let filter = parse_filter(params);
        let limit = i64_or(params, "limit", i64_or(params, "n", SCAN_LIMIT)).clamp(1, SCAN_MAX);
        let aggregate = parse_aggregate(params.get("aggregate"));
        let mut out = self
            .query_store(env)?
            .query_scan(source, &filter, aggregate, limit)
            .map_err(|error| error.to_string())?;
        if let Some(object) = out.as_object_mut() {
            object.insert("env".to_string(), json!(env));
            object.insert("source".to_string(), json!(source.label()));
        }
        Ok(out)
    }
}

fn parse_filter(params: &Value) -> QueryFilter {
    let filter = params.get("filter").cloned().unwrap_or(Value::Null);
    QueryFilter {
        session: opt_text(&filter, "session"),
        kind: opt_text(&filter, "kind"),
        topics: strings(&filter, "topics"),
        status: opt_text(&filter, "status"),
        name: opt_text(&filter, "name"),
        level: opt_text(&filter, "level"),
        since: filter.get("since").and_then(Value::as_i64),
        until: filter.get("until").and_then(Value::as_i64),
    }
}

fn parse_aggregate(value: Option<&Value>) -> Option<Aggregate> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    Some(Aggregate {
        op: text_or(value, "op", "count"),
        field: opt_text(value, "field"),
        group_by: opt_text(value, "group_by"),
    })
}
