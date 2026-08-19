use crate::config::{BenchTask, Benchmark};
use crate::drive::Drive;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(250);

/// One benchmark pass over the configured task set, run through that env's own
/// agent loop: the same seam a human uses, so a candidate cannot pass the gate
/// by being fast at something the harness never does.
#[derive(Debug, Clone, Default)]
pub struct Score {
    pub tasks: i64,
    pub passed: i64,
    pub rate: f64,
    pub cost: i64,
    pub failures: Vec<String>,
}

impl Score {
    pub fn json(&self) -> Value {
        json!({
            "tasks": self.tasks,
            "passed": self.passed,
            "success_rate": self.rate,
            "cost": self.cost,
            "failures": self.failures,
        })
    }

    pub fn row(&self) -> (i64, i64, f64, i64) {
        (self.tasks, self.passed, self.rate, self.cost)
    }
}

/// The label a run is filed under and compared within: numbers from a fake
/// model and numbers from a real one are not the same measurement, so they
/// never compare against each other.
pub fn label(config: &Benchmark) -> String {
    match config.model.as_str() {
        "real" => "real".to_string(),
        other => other.to_string(),
    }
}

pub async fn run(drive: &Drive) -> Score {
    let mut score = Score {
        tasks: drive.bench.tasks.len() as i64,
        ..Default::default()
    };
    let limit = Duration::from_secs(drive.bench.timeout_s.max(5));
    for task in &drive.bench.tasks {
        match one(drive, task, limit).await {
            Ok(cost) => {
                score.passed += 1;
                score.cost += cost;
            }
            Err((reason, cost)) => {
                score.cost += cost;
                score.failures.push(reason);
            }
        }
    }
    score.rate = match score.tasks {
        0 => 1.0,
        tasks => score.passed as f64 / tasks as f64,
    };
    score
}

async fn one(drive: &Drive, task: &BenchTask, limit: Duration) -> Result<i64, (String, i64)> {
    let created = drive
        .svc("loop", "session.create", json!({}))
        .await
        .map_err(|error| (format!("session.create: {error}"), 0))?;
    let session = created["session_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if session.is_empty() {
        return Err((
            "session.create answered without a session_id".to_string(),
            0,
        ));
    }
    let prompt = json!({"session_id": session, "text": task.prompt});
    drive
        .svc("loop", "session.prompt", prompt)
        .await
        .map_err(|error| (format!("session.prompt: {error}"), 0))?;
    let status = settle(drive, &session, limit)
        .await
        .map_err(|error| (error, 0))?;
    let cost = status["usage"]["total"].as_i64().unwrap_or(0);
    match graded(drive, task, &session, &status).await {
        Ok(()) => Ok(cost),
        Err(reason) => Err((format!("{}: {reason}", task.prompt), cost)),
    }
}

async fn settle(drive: &Drive, session: &str, limit: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + limit;
    let mut last = Value::Null;
    while Instant::now() < deadline {
        last = drive
            .svc("loop", "session.status", json!({"session_id": session}))
            .await?;
        let running = last["running"].as_bool().unwrap_or(false);
        let queued = last["queued"].as_i64().unwrap_or(0);
        if !running && queued == 0 {
            return Ok(last);
        }
        tokio::time::sleep(POLL).await;
    }
    Err(format!(
        "the turn did not finish inside the benchmark timeout: {last}"
    ))
}

async fn graded(
    drive: &Drive,
    task: &BenchTask,
    session: &str,
    status: &Value,
) -> Result<(), String> {
    if let Some(wanted) = &task.expect_substring {
        let answer = status["last"].as_str().unwrap_or_default();
        if !answer.contains(wanted.as_str()) {
            return Err(format!("the answer does not carry {wanted:?}: {answer:?}"));
        }
    }
    if task.tool_calls.is_empty() {
        return Ok(());
    }
    let history = drive
        .svc("loop", "session.history", json!({"session_id": session}))
        .await?;
    let called: Vec<String> = history["events"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| row["kind"] == json!("tool/call"))
                .filter_map(|row| row["data"]["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for wanted in &task.tool_calls {
        if !called.contains(wanted) {
            return Err(format!("the turn never called {wanted}, only {called:?}"));
        }
    }
    Ok(())
}

/// The gate itself: a candidate may not score worse than what the LKG scored,
/// and may not cost more than `cost_tolerance` times as much. With no LKG row
/// to compare against, a pass is whatever the candidate managed — the first
/// run of a benchmark set is a baseline, not a verdict.
pub fn compare(
    candidate: &Score,
    lkg: Option<(f64, i64)>,
    tolerance: f64,
) -> Result<Value, String> {
    let Some((rate, cost)) = lkg else {
        return Ok(json!({"compared": false, "candidate": candidate.json()}));
    };
    if candidate.rate + f64::EPSILON < rate {
        return Err(format!(
            "the benchmark scored {:.2} with the canary and {rate:.2} at the last known good: {:?}",
            candidate.rate, candidate.failures
        ));
    }
    if cost > 0 && candidate.cost as f64 > cost as f64 * tolerance.max(1.0) {
        return Err(format!(
            "the benchmark cost {} with the canary and {cost} at the last known good, past the \
             {tolerance}x tolerance",
            candidate.cost
        ));
    }
    Ok(json!({
        "compared": true,
        "candidate": candidate.json(),
        "lkg": {"success_rate": rate, "cost": cost},
    }))
}
