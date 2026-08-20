use crate::base::Base;
use crate::rpc::Cmd;
use serde_json::json;
use std::time::Duration;

impl Base {
    /// One arm per front-door command, and nothing else: the actor's own loop
    /// is in `base.rs`, the bodies are in the module that owns each subject.
    pub(crate) async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Boot { reply } => {
                let _ = reply.send(self.boot());
            }
            Cmd::Register {
                peer,
                role,
                env,
                pid,
                token,
                reply,
            } => self.on_register(peer, role, env, pid, token, reply),
            Cmd::Snapshot { reply } => {
                let _ = reply.send(self.snapshot());
            }
            Cmd::PeerOf { env, reply } => {
                let peer = self.nodes.get(&env).and_then(|node| node.peer.clone());
                let _ = reply.send(peer);
            }
            Cmd::Reset { env, probes, reply } => {
                if !probes.is_empty() {
                    self.emit_env(&env, "guardian.reset", json!({"probes": probes}));
                }
                let outcome = self.reset(&env).await;
                let _ = reply.send(outcome);
            }
            Cmd::SandboxExec {
                env,
                cmd,
                args,
                timeout_ms,
                reply,
            } => self.sandbox_exec(env, cmd, args, timeout_ms, reply),
            Cmd::SandboxDestroy { env, reply } => self.sandbox_destroy(&env, reply),
            Cmd::SandboxReaped { count } => {
                self.emit("sandbox.reaped", None, json!({"count": count}));
            }
            Cmd::WorkerBoot { env } => self.worker_boot(&env),
            Cmd::HarnessBoot { .. }
            | Cmd::HarnessReady { .. }
            | Cmd::HarnessExit { .. }
            | Cmd::EventsAppend { .. }
            | Cmd::Query { .. }
            | Cmd::LogQuery { .. }
            | Cmd::Records { .. }
            | Cmd::ConfigGet { .. }
            | Cmd::ConfigPatch { .. }
            | Cmd::Approval { .. } => self.on_env_cmd(cmd),
            Cmd::RuntimeRegister {
                env,
                params,
                token,
                reply,
            } => self.runtime_register(&env, &params, &token, reply),
            Cmd::RuntimeProbed {
                runtime,
                outcome,
                reply,
            } => self.runtime_probed(*runtime, outcome, reply),
            Cmd::ApprovalList {
                status,
                limit,
                reply,
            } => {
                let _ = reply.send(self.approval_list(status.as_deref(), limit));
            }
            Cmd::ApprovalAnswer {
                id,
                decision,
                note,
                reply,
            } => {
                let _ = reply.send(self.approval_answer(id, &decision, note.as_deref()));
            }
            Cmd::ApprovalExpire { id } => self.approval_expire(id),
            Cmd::Guard { env, reply } => {
                let _ = reply.send(self.allow_prompt(&env));
            }
            Cmd::Halt { env, reason } => {
                let grace = Duration::from_millis(self.config.stop_grace_ms);
                self.harness_halt(&env, grace).await;
                self.emit_env(&env, "env.halt", json!({"reason": reason}));
            }
            Cmd::Kill { on, reason, reply } => {
                let outcome = self.kill_switch(on, reason).await;
                if let Some(reply) = reply {
                    let _ = reply.send(outcome);
                }
            }
            Cmd::BudgetTick => self.tick_budgets(),
            Cmd::Processes { env, count } => self.processes(&env, count),
            Cmd::WorkerReady { env, pid, error } => self.worker_ready(&env, pid, error),
            Cmd::SnapPull { env, reply } => self.snap_pull(&env, reply),
            Cmd::SnapList { env, reply } => {
                let _ = reply.send(self.snap_list(&env));
            }
            Cmd::SnapExport {
                env,
                path,
                approved,
                reply,
            } => self.on_snap_export(env, path, approved, reply),
            Cmd::SnapPacked {
                env,
                step,
                reference,
                bytes,
            } => self.snap_packed(&env, step, &reference, &bytes),
            Cmd::Spawn {
                peer,
                parent,
                overrides,
                approved,
                reply,
            } => self.on_spawn(peer, parent, overrides, approved, reply),
            Cmd::RuntimeStop { env, reply } => {
                let outcome = self.runtime_stop(&env).await;
                let _ = reply.send(outcome);
            }
            Cmd::UpgradePropose { .. }
            | Cmd::UpgradeStatus { .. }
            | Cmd::UpgradeList { .. }
            | Cmd::UpgradeApproved { .. }
            | Cmd::UpgradePhase { .. }
            | Cmd::UpgradeWorker { .. }
            | Cmd::KernelSwitch { .. }
            | Cmd::KernelReady { .. } => self.on_upgrade_cmd(cmd),
            Cmd::UpgradeBench {
                env,
                label,
                id,
                row,
                lkg,
                reply,
            } => {
                let _ = reply.send(self.upgrade_bench(&env, &label, id, row, lkg));
            }
            Cmd::KernelDrain { env } => self.kernel_drain(&env).await,
            Cmd::Restored { env, result, error } => self.restored(&env, result, error),
            Cmd::EnvStatus { env, reply } => {
                let _ = reply.send(self.env_status(&env));
            }
            Cmd::Stop { reply } => {
                // Destroy every env's sandbox instance before answering, so a
                // caller that trusts "ok" and force-kills base a moment later
                // (a test fixture's teardown, a supervisor's own timeout) never
                // races an in-flight `podman stop`/`rm -f` and orphans it.
                self.stop().await;
                let _ = reply.send(Ok(json!({"ok": true})));
            }
            Cmd::AbortBoot { reply } => {
                self.abort_boot().await;
                let _ = reply.send(Ok(json!({"ok": true})));
            }
            Cmd::Attach { peer } => {
                self.attached.insert(peer);
            }
            Cmd::Gone { peer } => self.on_gone(peer).await,
            Cmd::Ready { reply } => {
                let _ = reply.send(self.ready());
            }
            Cmd::ScopeCheck { env, token, reply } => {
                let ok = self
                    .nodes
                    .get(&env)
                    .map(|node| !token.is_empty() && node.runtime_token == token)
                    .unwrap_or(false);
                let _ = reply.send(ok);
            }
        }
    }
}
