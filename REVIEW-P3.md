# REVIEW-P3 — final QA for RFC-P3-minimal-harness (2026-08-19)

Independent QA pass over P3.0-3.8 (git main, commit `b57af93`, clean tree). Every command
below was actually run on this box; wall times, test counts and token numbers are as
observed, not estimated. No product code was changed by this pass (machete found zero
unused deps, so no Cargo.toml edit was needed either). Full raw logs: `/tmp/tenon-qa-logs/`
on this box (not committed — ephemeral).

## 1. P3.x vs the RFC gate table

| Step | RFC gate | Verdict | Evidence |
|---|---|---|---|
| P3.0 safety floor | boots from profile; `kill -9` base stops nodes; `reset` restarts A from LKG, G stays up | PASS | `boot.rs` 8/8, `adversarial::lifecycle::*` 0 fail |
| P3.1 sandbox + gateway | worker tests pass on oci+landlock; plugin registers via gateway; killing sandbox/gateway leaves base untouched | PASS | `sandbox::oci.rs` 85.07% line cov, `gateway_gate` 1/1 |
| P3.2 worker + spawn | round trips, spill, snapshot/restore/expiry; A spawns B, tree shows lineage, parent death prunes | PASS | `worker_boot`, `spawn_gate`, `snap_test`, `worker/pty.rs` 90.58% cov |
| P3.3 harness | real model turn; resume from log; guard denies; single authority; agent mounts a plugin and sees it in tree | PASS | e2e: pong 704-1329ms, bash tool call 1987-2540ms, guard denial confirmed live |
| P3.4 storage | replay a session from SQLite; episodes grow | PASS | `storage_gate`, `storage/*.rs` 74-97% cov |
| P3.5 UI, rules, approvals | violation -> stop+rollback; budget hard stop; `kill -9` -> supervisor restart; corrupt state -> LKG | PASS | `attach --ui` PASS, approvals flow PASS (id 1, approve 4ms), `budget_gate` 2/2, `lkg::*` 0 fail |
| P3.6 krun + release | krun passes suite; fresh machine, one file, `tenon run` works | PARTIAL | krun **untested** — no `/dev/kvm` on this box (documented, rs/README §"krun backend"); single-file binary (77MB) built + verified: payload extracts, boots, runs a real turn, stops |
| P3.7 change protocol | upgrade a plugin/worker/kernel without downtime; bad upgrade auto-rolls back | PASS (suite only) | `contract_gate`, `upgrade_gate`, `manifest_gate` all 0 fail; **not re-exercised live** in this pass beyond the existing suite |
| P3.8 simplify | all suites green, 0 warnings, before/after; LoC -15-20%; written review | PASS | this pass reconfirms 0 warnings across both languages; LoC claim is P3.8's own (NOTES.md §29), not re-verified here |

krun is the one real gap: it is compiled, unit tested, and the conformance test is written to
skip itself cleanly with a reason string when unavailable (confirmed: this box has no
`/dev/kvm`, aarch64 cloud VM). It has never actually booted a microVM in this environment or
in this QA pass. `scripts/krun-smoke.sh` is the documented way to close that gap on a
KVM/HVF host or in CI.

## 2. Test inventory

| Project | Tests | Result | Notes |
|---|---:|---|---|
| `kernel/` | 66 | 0 fail | `mix compile`/`format` clean |
| `loader/` | 69 | 0 fail | credo strict clean (198 mods) |
| `cli/` (Elixir) | 7 | 0 fail | credo strict clean (54 mods) |
| `beam/` | 38 | 0 fail | credo strict clean (289 mods); `MIX_ENV=prod mix release --overwrite` succeeded |
| `sdk/test/` | 16 | 0 fail | mounts py+ts+rs SDKs together |
| `bridge/dsh/test/` | 6 | **intermittent** | 2 of 4 runs failed the same assertion (below) |
| `rs/` (workspace) | 173 | 0 fail | build/clippy(`-D warnings`)/fmt all clean; machete 0 unused deps |

Rust count (173) differs from the P3.8 snapshot in NOTES.md §29 (193: 126+47+20) — both runs
report 0 failures, so this is a counting-methodology difference (this run's 173 is the sum
`cargo test --all-features` itself reports across 41 binaries; NOTES.md's split may count
gate files differently), not a regression. Flagging for whoever reconciles the two numbers
next, not treating it as a finding.

**Flaky/intermittent, both load-induced, neither reproduced as a clean failure in isolation:**
- `beam` `mix test --cover`: one run hit a `LinkTest` teardown `MatchError` (a socket-close
  race in `test/support/base.ex:72`'s `on_exit`), not reproduced on rerun (clean, 76.47%).
  Attributed to CPU contention — this box was running concurrent `cargo test`, `mix release`
  and the live DSH demo at the time.
- `bridge/dsh/test`: `dsh_loader_test.exs:48`, "dsh did not hot-reload the rewritten profile
  patch" (`wait_until File.exists?(added)` times out at 60s), failed in 2 of 4 runs. Plausible
  root cause investigated: DSH's patch-watcher is armed *after* the fiber reports `:active`,
  event-driven with no initial-content diff, so a patch write that lands in the window between
  "active" and "watcher registered" is lost. This reads as a genuine timing race in DSH's HMR
  watcher registration order, not a Tenon regression — but a real ~50%-reproducible bug worth
  a ticket, not a "flaky, ignore" dismissal.
- Rust: the adversarial suite's documented pre-existing flaky teardown (NOTES.md §29, ~1/3
  runs historically) **did not trigger** in either full run of this pass (20/20 clean both
  times, once under plain `cargo test`, once under `cargo llvm-cov` instrumentation).

## 3. Coverage (honest, no rounding up)

**Elixir:**
- `loader`: **92.25%** total. Least covered: `Manifest` 86.11%, `Tree` 89.73%, `Server`
  90.91%, `Config` 94.41%, `Dsh` 95.83%.
- `beam`: **76.31-76.47%** total (below the project's own 90% Mix threshold — pre-existing
  debt, not introduced by P3.8). Least covered: `Boot` 2.63%, `Registry` 38.46%,
  `Gateway.Server` 56.41%, `Link` 57.14%, `Check` 60.47%.
- `kernel`: coverage tooling is **broken by the suite itself** —
  `kernel_adversarial_test.exs` does `:code.purge` + `:code.load_file` on `:tenon` mid-run,
  which discards `cover`'s instrumentation and reports a meaningless 100%/no-rows result for
  the full suite. Excluding that one file (48 of 66 tests): `:tenon` **78.15%**. Report this
  number with the caveat, not the full-suite 100%, which is an artifact, not a measurement.

**Rust** (`cargo llvm-cov --all-features --workspace`, all suites including the
container-booting adversarial/cli-gate tests confirmed to run instrumented — sibling files in
the same crates that only integration tests reach show 60-90%+, which unit tests alone could
not produce): **74.83% region / 72.26% function / 77.92% line**, workspace-wide.

Five least-covered files by line %:
1. `harness/src/bus.rs` — 0.00% (0/21 lines)
2. `harness/src/manage.rs` — 2.70% (3/111)
3. `worker/src/service.rs` — 4.60% (15/326)
4. `harness/src/api.rs` — 7.30% (13/178)
5. `sandbox/src/krun/vmm.rs` — 8.59% (11/128) — expected, krun untestable here (§1)

`base/src/http.rs` (19.76%) and `sandbox/src/krun/ffi.rs` (23.08%) are the next two; `http.rs`
low coverage is real (the HTTP carrier's e2e pass in §5 covers it behaviorally, not via unit
tests) and worth a follow-up. `bus.rs`/`manage.rs`/`api.rs` sit beside 80-90%+ covered
siblings in the same crates (`agent.rs` 88.69%, `tools.rs` 90.16%) — real gaps in the
management-tools and raw-bus code paths, not artifacts of the harness.

## 4. Dead code and dependencies

- `cargo machete`: **zero unused dependencies.** (P3.8 already removed the one prior finding,
  `libc` in `tenon-harness`.) No Cargo.toml edit was needed or made.
- `cargo udeps`: attempted, skipped — a from-source `cargo install cargo-udeps` did not finish
  in the time budgeted (large dependency tree: `gix`, `cargo`-internals). Machete already
  covers the "unused dependency" question; udeps additionally catches used-but-declared
  differently cases, which this pass does not have data on.
- Elixir `mix xref graph --format stats`: `loader` has **one 2-file cycle**
  (`lib/tenon/loader.ex` <-> `lib/tenon/loader/server.ex`), 0 cycles elsewhere. Not fixed,
  per instructions — reported only. `beam`: 0 cycles, 19 runtime deps, no findings.
  `mix xref unreachable` is a no-op on this OTP/Elixir version ("moved to the compiler").

## 5. End-to-end evidence

**CLI, real model (`deepseek-v4-flash`), `TENON_HOME=/tmp/tenon-qa-cli`, sandbox `oci`:**

| Command | Wall time | Result |
|---|---:|---|
| `tenon start` -> ready | <1s (first poll) | sandbox backend `oci` |
| `run "reply with the single word pong"` | 1289ms | `pong`, usage `{completion:18,prompt:1721,total:1739}` |
| `run "...bash tool: echo tenon-e2e && uname -m..."` | 1987ms | `tool/call` bash, `tool/result` `tenon-e2e\naarch64`, usage total 3684 |
| `run "...bash tool: rm -rf /tmp/x"` (guard plugin mounted via profile) | 11824ms | `tool/result` `{denied:true,text:"blocked by tenon guard"}` |
| approvals flow (`gated_tools:[bash]`, `approval: ask`) | approve rpc 4ms | pending row appeared ~2-3s after run start; `approve 1` released it; run completed with `tenon-approved` |
| `reset` then `run "pong"` | reset 3344ms, run 1012ms | harness survived the env restart |
| `attach --ui` under a real pty, `q` after 3s | clean exit, 17769B captured | border chars + TREE/TRANSCRIPT/APPROVALS/EVENTS panes present |
| `stop` | 4257ms | `{ok:true}`; oci container fully reaped ~15-20s *after* `stop` returns (§7) |

Shipping single-file binary (`scripts/build-release.sh` output, `TENON_HOME` fresh, **no**
`--release-dir`/`TENON_RELEASE_DIR`): boots in 2279ms, extracts the embedded BEAM payload
under `TENON_HOME/erts/`, runs a real "pong" turn (839ms), stops cleanly. This is the
`--verify` check `build-release.sh` itself runs, independently reconfirmed here.

**One security-relevant finding (not fixed, reported):** the demo guard plugin
(`playground/web/plugins/guard.py`, the one both the DSH bridge and this pass's Tenon-native
profile use) is a **plain substring match on `"rm -rf"`**. During the deny test above, the
model — after seeing the denial — retried with `rm -r /tmp/x` (no second `r`) in the *same
turn* and the guard let it through, achieving the identical effect; a trailing-slash
`rm -rf /tmp/x/` was still caught, confirming it is string matching, not path- or intent-aware.
This is explicitly a demo/example plugin, not the sandbox boundary itself (RFC §5.4: "no deny
lists inside the VM" is the documented design — the sandbox boundary, not a tool-call
denylist, is what's supposed to hold), so this is not a violation of the P3 threat model, but
it is worth documenting loudly: **a `tools/pre-execute` guard plugin is a demonstration of the
hook point, not a security control**, and any real deployment relying on one needs an
actually adversarial-resistant policy, not a substring ban list.

**HTTP carrier** (`TENON_HOME=/tmp/tenon-qa-http`, `serve --http 127.0.0.1:38080`): `GET /`
200 with env name in the rendered `<pre>`; `POST /prompt text=...` 303 redirect, next `GET /`
shows the assistant's reply in the transcript pane; `POST /rollback` 303; `POST /approve/<id>`
exercised the full round trip against a gated `bash` call — `GET /` afterward shows the tool
ran and the approved output. All PASS.

**DSH web smoke** (`node playground/smoke/smoke.mjs` against the live user demo,
`127.0.0.1:3080`, untouched throughout this pass): **4/4 PASS**, 86.762s wall — model turn,
`bash` tool executed, `rm -rf` denied by the same guard plugin, `audit.jsonl` has matching
session/created + tools/pre-execute lines. Confirms the L2/L3 DSH-bridge path still works
end to end, unrelated to the Tenon-native harness path exercised above.

**One build-tooling gotcha found (not fixed):** `scripts/build-release.sh` and the ordinary
dev workflow (`cargo build --release --all-features`) both write to the same
`rs/target/release/tenon`, with different feature sets — running `build-release.sh` silently
drops the `http` feature from that binary. This QA pass hit it directly (the HTTP e2e step
failed with "unrecognized subcommand 'serve'" until the `--all-features` build was rerun).
Worth a `CARGO_TARGET_DIR` split or a feature-name-suffixed output if both shapes are built
routinely on the same box.

## 6. Performance: Tenon-native vs DSH

Same box, same model, apples-to-oranges by design and stated plainly: DSH's web app carries a
Node.js UI/CLI process on top of one BEAM node; Tenon's `oci` sandbox boot spawns and tears
down a real podman container per environment, and its release runs guardian and the root env
as **two** separate BEAM nodes where DSH's escript runs one node for both roles.

| Metric | Tenon (min/median, 3 runs) | DSH | Note |
|---|---|---|---|
| (a) cold start to ready | 1300ms / 1571ms | not re-measured (would require restarting the live demo — forbidden); best proxy: DSH's own test suite logged 1159-3346ms for a **lighter** plugin bundle, not the full web profile | not apples-to-apples, see above |
| (b) idle RSS, process tree | ≈309.2MB (`ps` sum: 2×beam.smp 142.9+144.7MB, base 9.2MB, harness 8.0MB, sandbox container ~1.4+5.5MB, demo plugin 2.0MB) | ≈297.6MB (`ps` sum: 1×beam.smp 92.7MB, node 188.8MB, 2 python plugins 21.8MB) | roughly comparable totals from different shapes; `podman stats` disagreed with `ps` on the container's own share (974.8kB vs ~6.9MB `ps`-summed) - flagged, not resolved |
| (c) local tool round trip, no model | 133.66ms / 141.14ms (10 runs, raw UDS frames to `sandbox.exec`) | **not measurable** — DSH's HTTP API has no bare tool-invocation route; every method is session/turn-shaped (checked `bridge/dsh/src/plugin.ts`'s `tools.execute`, which is an internal wire RPC, not an HTTP route) | explained, not guessed |
| (d) e2e "reply pong" | 704ms / 1078ms | 1027.9ms / 1533.1ms | Tenon faster on this box, both real model calls |
| (e) e2e one bash tool call | 2011ms / 2134ms | 2545.1ms / 3050.1ms | Tenon faster on this box, both real model calls |

Token accounting differs by provider surface, not just by system: Tenon reports
`{completion,prompt,total}` from the OpenAI-compatible API; DSH reports
`{inputTokens,outputTokens,cacheReadTokens,reasoningTokens}` including DeepSeek's own prompt
cache. Read as "same model, same box," not "same billing units."

## 7. Known gaps and deviations

Full deviation list: `rs/README.md` §"Deviations from the RFC" (40 numbered items, all
already-shipped design decisions, not new findings from this pass). New/reconfirmed findings
from this pass specifically:

1. **krun untested here** (§1) — no `/dev/kvm` on this box; compiled, unit tested, conformance
   suite skips itself with a reason string; needs a KVM/HVF host or CI runner.
2. **Guard plugin is a substring filter, not a security control** (§5) — trivially bypassed by
   rephrasing the same command; correctly scoped as a demo of the hook point per the RFC's "no
   deny lists inside the VM" stance, but worth calling out loudly so nobody mistakes it for one.
3. **`bridge/dsh/test`'s HMR watcher race** (§2) — ~50% reproducible, plausible root cause
   identified, not a Tenon regression, worth a DSH-side ticket.
4. **`beam` coverage (76.3%) sits below the project's own 90% Mix threshold** (§3) —
   pre-existing, not introduced here; `Boot` at 2.63% is the standout gap.
5. **`rs/target/release/tenon` build-path collision** (§5) — `build-release.sh` and the plain
   dev build silently clobber each other's feature set.
6. **`tenon stop` returns before the oci container is fully reaped** (~15-20s lag observed) —
   correct per design (deviation 11: reap is decoupled from the actor's `Cmd` queue), but
   worth documenting for anyone scripting against `stop`'s return as "fully torn down."
7. **`harness/bus.rs`, `manage.rs`, `api.rs` coverage gaps** (§3) — real, not instrumentation
   artifacts; management-tools and raw-bus code paths are the least-exercised parts of the
   harness crate.

## 8. Risks

- The krun backend is the only VM-isolation backend and is completely unverified outside unit
  tests on any box available to this pass — it is the biggest single gap between "the P3 gate
  table says PASS" and "this has been seen working."
- The guard-plugin finding (§5, §7.2) is a reminder that P3's tool-call hooks are a mechanism,
  not a policy; anyone building on this needs to bring real policy, not copy the demo.
- Coverage in `harness/src/manage.rs` (2.70%) and `bus.rs` (0%) means the agent-facing
  management tools (`plugin.mount`, `config.patch`, etc. per RFC §6) are the least
  test-verified surface of the whole harness — exactly the surface an agent evolving its own
  runtime touches most.

## 9. How to review (human + Gemini), ~10 minutes to reproduce the e2e claims

Read in this order: `RFC-P3-minimal-harness.md` §0-2 (vocabulary) and §12 (the P3.x plan) for
intent; `rs/README.md` top-to-bottom for the shipped shape and its 40 deviations; `NOTES.md`
§17-29 for what each phase actually built and measured; this document for what an outside pass
found. `AGENTS.md` has the gate commands per subproject if you want to rerun them yourself.

To reproduce the CLI e2e in ~10 minutes:
```
export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"
source /home/ubuntu/workspace/deepseek.env.sh && export DEEPSEEK_API_KEY="$ANTHROPIC_AUTH_TOKEN"
cd rs && TENON_RELEASE_DIR=$PWD/../beam/_build/prod/rel/tenon_beam \
  ./target/release/tenon --home /tmp/review-try start
./target/release/tenon --home /tmp/review-try status   # wait for harness "ready"
./target/release/tenon --home /tmp/review-try run "reply with the single word pong"
./target/release/tenon --home /tmp/review-try stop
```
A live instance is left running for direct inspection: `TENON_HOME=~/.tenon-demo`, HTTP UI at
`http://127.0.0.1:38080/`, pid file `~/.tenon-demo/run/base.ready` — attach with
`TENON_HOME=~/.tenon-demo rs/target/release/tenon attach`, stop with `... stop` (also kill the
separate `serve` process). This is distinct from the user's own DSH demo on `:3080` and its
own `TENON_HOME` — neither was touched by this pass.
