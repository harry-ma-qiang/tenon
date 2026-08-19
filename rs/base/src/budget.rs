use crate::base::Base;
use crate::config::{Budgets, Prices};
use crate::node::GUARDIAN;
use crate::rpc::Cmd;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

pub const STOP_FILE: &str = "STOP";
const STOP_POLL_MS: u64 = 400;
const PROC_TIMEOUT: Duration = Duration::from_secs(10);
const PROC_COUNT: &str = "ls -1 /proc | grep -c '^[0-9][0-9]*$'";

/// What one env has spent since it booted. Cleared by `tenon reset` when
/// `budget_reset_on_reset` is on, which is the documented way back after a
/// hard stop.
#[derive(Debug, Clone, Default)]
pub struct Budget {
    pub tokens: i64,
    pub usd: f64,
    pub started: i64,
    pub halted: Option<String>,
}

impl Budget {
    pub fn json(&self, limits: &Budgets, wall_s: i64) -> Value {
        json!({
            "tokens": self.tokens,
            "usd": self.usd,
            "wall_s": wall_s,
            "limits": {
                "tokens": limits.tokens,
                "usd": limits.usd,
                "wall_s": limits.wall_s,
                "processes": limits.processes,
            },
            "halted": self.halted,
        })
    }
}

/// Watches `<home>/run/STOP` and turns its appearance and removal into the
/// kill switch's two commands. A file, a signal and an RPC are the three
/// carriers of the same thing (RFC section 5).
pub fn watch_stop_file(path: std::path::PathBuf, cmds: mpsc::UnboundedSender<Cmd>) {
    tokio::spawn(async move {
        let mut on = path.is_file();
        if on {
            let _ = cmds.send(Cmd::Kill {
                on: true,
                reason: format!("{} exists", path.display()),
                reply: None,
            });
        }
        loop {
            tokio::time::sleep(Duration::from_millis(STOP_POLL_MS)).await;
            let now = path.is_file();
            if now == on {
                continue;
            }
            on = now;
            let reason = match on {
                true => format!("{} exists", path.display()),
                false => format!("{} was removed", path.display()),
            };
            if cmds
                .send(Cmd::Kill {
                    on,
                    reason,
                    reply: None,
                })
                .is_err()
            {
                return;
            }
        }
    });
}

pub fn ticker(interval: Duration, cmds: mpsc::UnboundedSender<Cmd>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            if cmds.send(Cmd::BudgetTick).is_err() {
                return;
            }
        }
    });
}

impl Base {
    /// Base's own budgets merged with whatever the env's overlay overrides,
    /// so one env can be tighter than the host default and never looser than
    /// its own config says.
    pub fn budgets_of(&self, env: &str) -> Budgets {
        let mut limits = self.config.budgets;
        let overlay = self.home.harness_config(env).unwrap_or_else(|_| json!({}));
        let Some(rows) = overlay.get("budgets").and_then(Value::as_object) else {
            return limits;
        };
        if let Some(value) = rows.get("tokens").and_then(Value::as_i64) {
            limits.tokens = value;
        }
        if let Some(value) = rows.get("usd").and_then(Value::as_f64) {
            limits.usd = value;
        }
        if let Some(value) = rows.get("wall_s").and_then(Value::as_u64) {
            limits.wall_s = value;
        }
        if let Some(value) = rows.get("processes").and_then(Value::as_i64) {
            limits.processes = value;
        }
        limits
    }

    /// `usd_per_1k: {input, output}`, or a per-provider table keyed by the
    /// name the env's `llm.provider` carries.
    pub fn prices_of(&self, env: &str) -> Prices {
        let overlay = self.home.harness_config(env).unwrap_or_else(|_| json!({}));
        let Some(table) = overlay.get("usd_per_1k") else {
            return self.config.usd_per_1k;
        };
        let provider = overlay
            .get("llm")
            .and_then(|llm| llm.get("provider"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let row = match table.get("input").is_some() || table.get("output").is_some() {
            true => table.clone(),
            false => table.get(provider).cloned().unwrap_or(Value::Null),
        };
        Prices {
            input: row.get("input").and_then(Value::as_f64).unwrap_or(0.0),
            output: row.get("output").and_then(Value::as_f64).unwrap_or(0.0),
        }
    }

    /// Every step's usage as the llm adapter reported it, read off the session
    /// log on its way into the env's state file. The log is the truth, so the
    /// counter cannot disagree with what the model actually cost.
    pub fn account(&mut self, env: &str, kind: &str, data: &Value) {
        if kind != "assistant/message" {
            return;
        }
        let usage = &data["usage"];
        let prompt = usage["prompt"].as_i64().unwrap_or(0);
        let completion = usage["completion"].as_i64().unwrap_or(0);
        let total = match usage["total"].as_i64().unwrap_or(0) {
            0 => prompt + completion,
            given => given,
        };
        if total == 0 && prompt == 0 && completion == 0 {
            return;
        }
        let prices = self.prices_of(env);
        let spent =
            (prompt as f64 / 1000.0) * prices.input + (completion as f64) / 1000.0 * prices.output;
        if let Some(node) = self.nodes.get_mut(env) {
            node.budget.tokens += total;
            node.budget.usd += spent;
        }
        self.check_budget(env);
    }

    /// The hard stop of RFC section 5: over a limit, the env's harness is
    /// halted, the guardian is told and every further prompt is refused with
    /// the reason until a `reset`.
    pub fn check_budget(&mut self, env: &str) {
        if self.nodes.get(env).map(|node| node.budget.halted.is_some()) != Some(false) {
            return;
        }
        let limits = self.budgets_of(env);
        let Some(node) = self.nodes.get(env) else {
            return;
        };
        let wall = match node.budget.started {
            0 => 0,
            started => (tenon_storage::now() - started) / 1000,
        };
        let breach = if limits.tokens > 0 && node.budget.tokens >= limits.tokens {
            Some(("tokens", json!(node.budget.tokens), json!(limits.tokens)))
        } else if limits.usd > 0.0 && node.budget.usd >= limits.usd {
            Some(("usd", json!(node.budget.usd), json!(limits.usd)))
        } else if limits.wall_s > 0 && wall >= limits.wall_s as i64 {
            Some(("wall_s", json!(wall), json!(limits.wall_s)))
        } else {
            None
        };
        if let Some((what, used, limit)) = breach {
            self.exceeded(env, what, used, limit);
        }
    }

    pub fn exceeded(&mut self, env: &str, what: &str, used: Value, limit: Value) {
        let reason = format!("budget {what} exhausted: {used} of {limit}");
        if let Some(node) = self.nodes.get_mut(env) {
            if node.budget.halted.is_some() {
                return;
            }
            node.budget.halted = Some(reason.clone());
        }
        self.emit_env(
            env,
            "budget.exceeded",
            json!({"budget": what, "used": used, "limit": limit, "reason": reason}),
        );
        self.notify_guardian(
            "budget.exceeded",
            json!({"env": env, "budget": what, "reason": reason}),
        );
        let _ = self.cmds.send(Cmd::Halt {
            env: env.to_string(),
            reason,
        });
    }

    /// The sandbox's own process count, asked for on the budget tick and only
    /// when a limit is configured: it is a container round trip, not a read.
    pub fn tick_budgets(&mut self) {
        let envs: Vec<String> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.role != GUARDIAN)
            .map(|(env, _)| env.clone())
            .collect();
        for env in envs {
            self.check_budget(&env);
            let limits = self.budgets_of(&env);
            if limits.processes <= 0 {
                continue;
            }
            let Some(node) = self.nodes.get(&env) else {
                continue;
            };
            if node.budget.halted.is_some() {
                continue;
            }
            let Some(instance) = node.sandbox.clone() else {
                continue;
            };
            let cmds = self.cmds.clone();
            let name = env.clone();
            tokio::task::spawn_blocking(move || {
                let outcome = instance.exec(
                    "sh",
                    &["-c".to_string(), PROC_COUNT.to_string()],
                    PROC_TIMEOUT,
                );
                let count = outcome
                    .ok()
                    .and_then(|out| {
                        String::from_utf8_lossy(&out.stdout)
                            .trim()
                            .parse::<i64>()
                            .ok()
                    })
                    .unwrap_or(0);
                let _ = cmds.send(Cmd::Processes { env: name, count });
            });
        }
    }

    pub fn processes(&mut self, env: &str, count: i64) {
        let limits = self.budgets_of(env);
        if limits.processes <= 0 || count <= limits.processes {
            return;
        }
        self.exceeded(env, "processes", json!(count), json!(limits.processes));
    }

    /// The kill switch itself: every harness stops, every prompt is refused,
    /// and nothing comes back until the file is gone or `resume` is called.
    pub async fn kill_switch(&mut self, on: bool, reason: String) -> Result<Value, String> {
        if on {
            if self.killed.is_none() {
                self.killed = Some(reason.clone());
                self.emit("kill.switch", None, json!({"on": true, "reason": reason}));
                self.notify_guardian("kill.switch", json!({"on": true, "reason": reason}));
            }
            let envs: Vec<String> = self
                .nodes
                .iter()
                .filter(|(_, node)| node.role != GUARDIAN)
                .map(|(env, _)| env.clone())
                .collect();
            let grace = Duration::from_millis(self.config.stop_grace_ms);
            for env in envs {
                self.harness_halt(&env, grace).await;
            }
            return Ok(json!({"ok": true, "killed": true, "reason": self.killed}));
        }
        if self.killed.take().is_some() {
            self.emit("kill.resume", None, json!({"reason": reason}));
            self.notify_guardian("kill.resume", json!({"reason": reason}));
            let envs: Vec<String> = self
                .nodes
                .iter()
                .filter(|(_, node)| node.role != GUARDIAN && node.registered)
                .map(|(env, _)| env.clone())
                .collect();
            for env in envs {
                self.harness_boot(&env);
            }
        }
        Ok(json!({"ok": true, "killed": false}))
    }

    /// What every prompt passes first. A halted env and a killed base refuse
    /// with the reason rather than with silence.
    pub fn allow_prompt(&self, env: &str) -> Result<Value, String> {
        if let Some(reason) = &self.killed {
            return Err(format!("the kill switch is on: {reason}"));
        }
        match self
            .nodes
            .get(env)
            .and_then(|node| node.budget.halted.clone())
        {
            Some(reason) => Err(format!("env {env} is halted: {reason}")),
            None => Ok(json!({"ok": true})),
        }
    }

    pub fn halted(&self, env: &str) -> bool {
        self.killed.is_some()
            || self
                .nodes
                .get(env)
                .map(|node| node.budget.halted.is_some())
                .unwrap_or(false)
    }

    pub fn budget_view(&self, env: &str) -> Value {
        let limits = self.budgets_of(env);
        let Some(node) = self.nodes.get(env) else {
            return Value::Null;
        };
        let wall = match node.budget.started {
            0 => 0,
            started => (tenon_storage::now() - started) / 1000,
        };
        node.budget.json(&limits, wall)
    }
}
