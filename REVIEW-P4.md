# REVIEW-P4 — final QA for RFC-P4-plumbing (2026-08-20)

Independent QA pass over P4.0-4.8 (git main, clean tree at the P4.8 commits). Every command
below was actually run on this box (aarch64 cloud VM, no `/dev/kvm`); wall times, test counts and
throughput numbers are as observed, not estimated. The e2e ran against a fresh temp home with the
**single-file release binary** (76 MB, embedded BEAM payload, built `--features http`) and the real
`deepseek-v4-flash` model; the key travels only as `$DEEPSEEK_API_KEY` (set from the shell env), is
never printed, logged or committed. Temp homes were `/tmp/tenon-qa-p4*` and were stopped and removed;
the live demo base (`~/.tenon-demo`, pid 647207, `:38080`) was left running and untouched.

One caveat stated up front: a second agent was running the integration suites against this same
checkout throughout the pass. Where that concurrency mattered — the load-induced adversarial flake,
the RSS number, the throughput figure — it is called out explicitly rather than smoothed over.

## 1. P4.x vs the RFC plan table

| Step | RFC gate | Verdict | What shipped |
|---|---|---|---|
| P4.0 bus/kv/blob/timer | section 4 budgets by bench; durable replay after `kill -9`; a cron/after timer fires and survives restart | PASS | `rs/bus` (envelope, Hub, rings, tracing layer) + base facades; env-scoped subscribe/kv/blob (8d.2); timer wheel in durable kv. `bus_gate` green; bench clears budgets (§6) |
| P4.1 migration onto facades | net LoC recorded; suites green; UI on subscribe, no polls | PASS | one `publish_event` fan-out replaces the P4.0 durable bridge **and** base's own subscriber list; `subscribe` RPC + `subs` map deleted; `tenon run`/`attach`/`--ui`/`serve` all stream from `bus.subscribe`. Net base src +150 (honestly not negative; see NOTES §P4.1) |
| P4.2 query hot layer | text < 10 ms, scan < 100 ms at 1M | PASS | `query.text` (FTS5, bm25, rebuildable derived index), `query.scan` (typed, allowlisted), `query.vector` stub; `.tail` read RPC families deleted. 1M perf: text p99 **0.48 ms**, scan p99 **70 ms** (§6) |
| P4.3 warm segments | 10M budgets; rebuild-from-log | **SKIPPED (deliberate)** | Parquet/Tantivy compactor not built. RFC section 5 says it "may land after P5 starts"; the query facade is the stable seam, the engine is P5's. Recorded as a known deviation, not a gap discovered here |
| P4.4 serve hardening | https works; no token = 401; WS subscribe; secrets mask/block; feature-off unchanged | PASS | one `authorize()` for every carrier; `--https` (rustls+rcgen self-signed); WS 5th carrier; secrets facade with hub-level mask/block. Feature-off adds no dep (metadata-only binary delta, NOTES §P4.4) |
| P4.4 security | env isolation on every carrier; no tag leaks | PASS | 5 adversarial defects found + fixed (§4); one default-deny `enforce_scope` for every method |
| P4.5 ingress | in-sandbox app reachable via https+token; killing it expires the route; a 2nd env can't claim the name | PASS | `ingress.register/list`, lease-backed kv routes, `/app/<name>` proxy (WS pass-through), oci fixed port span. `ingress_gate` green in 13.4 s (§5) |
| P4.6 backup/restore | backup under load restores to a working home; checksums match | PASS | `VACUUM INTO` per state file + `config.yml`/`profiles/`/LKG + checksummed `backup.json`; restore refuses over a live base and on drift. Live e2e replay confirmed (§5). The P4.6 fixture race (unique-home fix) did **not** recur |
| P4.7 triggers/webhook/MCP | external MCP tool callable through the loop + denied by a guard; Tenon-as-MCP runs bash in the sandbox | PASS | triggers (publish/http_post/prompt + hop guard), `POST /hook/<topic>`, MCP bridge both directions. `mcp_gate` + `trigger_gate` + `webhook_gate` green; both MCP directions e2e'd (§5) |
| P4.8 doctor + shared probes | one shared probe list; doctor one-shot offline; guardian loop unchanged; docs + REVIEW + NOTES | PASS | `tenon doctor` (7 install probes, `rs/base/src/doctor.rs`); the guardian's runtime probes converged into one documented catalog (`@catalog` in `probes.ex`) doctor mirrors by name. Guardian suite unchanged and green |

P4.3 is the one intentional non-delivery, exactly as the RFC allows. Everything else in the plan
table shipped and is gated.

## 2. Test inventory (after P4)

| Project | Tests | Result | Notes |
|---|---:|---|---|
| `beam/` | 51 | 0 fail | `mix compile --warnings-as-errors`, `mix format --check-formatted`, `mix credo --strict` (386 mods/funs) all clean |
| `rs` unit (lib) | 101 | 0 fail | base 30 (**+3 doctor**), bus 18, sandbox 17, storage 18, ui 18; harness/worker/test-support carry their tests as integration binaries |
| `rs` integration | see §5 | 0 fail (isolation) | per-gate, run individually and in one parallel pass; the adversarial suite is 20/20 in isolation |
| `rs` build/lint | — | clean | `cargo build --release` (feature off) + `--features http`, `clippy --all-targets --all-features -D warnings`, `fmt --check` all clean |

Integration gates confirmed green this pass (individually, release, oci): `guardian_gate`,
`ingress_gate` (13.4 s), `mcp_gate` (2, both directions), plus the batch in §5. The full parallel
`cargo test --features http` run finished with the whole suite passing **except** two
`adversarial::crash::*` tests that tripped while the concurrent second agent's release build was
saturating the CPU ("root never came back" — a guardian-reset timing deadline missed under load).
Re-run in isolation the adversarial suite is **20/20 clean (188 s)**. This is the documented
pre-existing load-induced adversarial flakiness (REVIEW-P3 §2), not a regression and not the P4.6
fixture race — that race, the specific target of the P4.6 unique-home fix, did **not** recur.

## 3. Coverage (honest note)

A fresh full-workspace `cargo llvm-cov` was **not** re-run this pass: it re-instruments and re-runs
every container-booting gate (~20 min clean), and with a second agent already hammering the shared
`target/` dir the number would have been both slow to get and unfair to report. The comparable P3
baseline stands (REVIEW-P3 §3: **77.92 % line / 74.83 % region** workspace-wide). What changed for
P4: the new surface (`rs/bus`, the query hot layer, the facades, ingress, triggers, MCP, backup,
doctor) each ships with its own integration gate that drives it end to end, and the new
`doctor.rs` carries three unit tests covering all its ok/warn/fail branches (clean home, corrupt
state file, missing-config-writes-nothing). The least-covered files flagged in P3
(`harness/bus.rs`, `manage.rs`, `base/http.rs`) were not a P4 target and are unchanged.

## 4. Security review

P4 adds a large listener surface (https, WS, ingress, webhook, MCP), and the RFC's rule (8d) is that
every one reuses **one** boundary. An adversarial pass (`ws_scope_adversarial`,
`serve_authz_adversarial`, `secrets_leak_adversarial`, beam `gateway_ws_adversarial`) found and fixed
five real defects:

1. **WS carrier was unscoped (CRITICAL).** The `/ws` bridge opened base's front door unbound, so a
   browser client with only the shared bearer token rode in host-wide. Fix: `serve` is env-bound by
   default — the bridge calls `auth.scope{env, token}` before forwarding any frame; `--admin` opts
   out for a deliberate barebone carrier.
2. **`secret.get` grant was meaningless over WS (CRITICAL).** A WS caller had no bound scope, so
   grants never applied. Fixed for free by #1.
3. **Dispatch-level scope gap (HIGH).** `query./bus./kv./blob./timer.` checked scope; `config.get`,
   `session.*`, `svc` did not. Fix: one **default-deny** `facaderpc::enforce_scope`, called by
   `server::dispatch` for **every** method — env-safe methods force env == bound, barebone-only
   methods refuse when scoped, and an unclassified new method is barebone-only (deny) by default.
4. **Secret leak via tags (HIGH).** The hub's leak scan walked only `payload`; a value in `tags`
   sailed through. Fix: `scan_envelope` scans payload **and** tag keys+values before any masking.
5. **Split-payload across envelopes (MEDIUM, documented).** Substring scanning has no cross-envelope
   memory; a value split over two durable envelopes is not caught. Fixed the honest way — documented
   as a producer-side-scrub limit and pinned by a test that asserts the limitation, not the
   impossible.

The load-bearing invariant is #3's **one default-deny scope guard**: adding a route can never add an
auth path, and a method nobody classified is refused to a scoped caller rather than waved through.

Note (carried from P3, still true): the demo `guard.py` is a substring match on `"rm -rf"`, a
demonstration of the `tools/pre-execute` hook point, **not** a security control — the sandbox
boundary is. Anyone building policy on a tool-call denylist is mistaken about where the wall is.

## 5. End-to-end evidence (single-file binary, real model)

Fresh home `/tmp/tenon-qa-p4`, binary `/tmp/tenon-e2e` (embedded payload, no `--release-dir`), oci
sandbox, model `deepseek-v4-flash`:

| Step | Result |
|---|---|
| `tenon doctor` (before start) | 6 ok / 1 warn (release: payload not yet extracted) / 0 fail, exit 0 |
| `tenon start` | base ready **1470 ms**; harness ready ~2 s; erts extracted from the embedded payload |
| `tenon doctor` (after start) | 7 ok / 0 fail — release ok, `state_integrity` "2 state files pass integrity_check" |
| bus / kv / timer (raw UDS frame client) | `kv.set` rev 1, `kv.get` → `world`; `bus.subscribe`+`bus.publish` delivered envelope `e2e/ping {n:42}`; `timer.set after_ms:800` fired `e2e/tick {tick:1}` |
| `tenon run "reply … pong"` | `pong`, **1142 ms**, usage `{completion:3,prompt:1721,total:1724}` |
| `tenon run` bash tool | `tool bash ok`, output `tenon-p4-e2e` + `aarch64`, **2026 ms** |
| MCP server (`tenon mcp` stdio) | `initialize` → serverInfo `tenon 0.1.0`; `tools/list` → 12 tools; `tools/call bash` → `mcp-roundtrip-ok`, `isError:false` |
| MCP client (mount a python MCP server) | `mcp_gate`: a mounted MCP tool is callable through the loop and **denied by a guard hook** |
| webhook `POST /hook/e2e-hook` (https) | no token → **401**; with token → **200** `{ok:true}`; self-signed cert fingerprint printed |
| ingress `/app/<name>` (https+token) | `ingress_gate`: in-sandbox python app reachable with the token, `X-Tenon-*` echoed, no token 401, a 2nd env refused the owned name, `kill -9` of the app expires the route |
| backup + restore replay | `backup` (under a live base) → 10 files; `restore` into a **fresh** home → 10 files; restart there replays the session log — **23 events**, 2 `user/message`, 3 `assistant/message`, 2 `turn/end` |

Batch of remaining P4 gates run this pass (release, oci, individually): `bus_gate`, `query_gate`,
`ws_gate`, `secrets_gate`, `backup_gate`, `webhook_gate`, `trigger_gate`, `serve_https_gate`,
`serve_authz_adversarial`, `ws_scope_adversarial`, `secrets_leak_adversarial` — see the run log; all
green. (ingress was verified via its gate rather than driven by hand because the in-sandbox
registration path has no CLI — the app must speak the gateway `link` service from inside the box.)

## 6. Performance

Measured on this box; the two throughput/latency figures were taken while the second agent's suite
was also running, so they are floors, not ceilings.

| Metric | Number | Budget | Note |
|---|---|---|---|
| bus fan-out throughput | **282,141 msg/s** (100k envelopes, 354 ms) | 100k/s | under concurrent CPU load; NOTES §P4.0 saw 344k idle |
| bus publish→subscriber latency | p50 **1.04 µs**, p99 **1.32 µs** | p99 < 1 ms | ~750x headroom even under load |
| query `text` at 1M events | p50 402 µs, p99 **477 µs** | < 10 ms | FTS5 + bm25; index rebuilt from the log in 6.3 s |
| query `scan` (count group_by, 1M) | p50 68.8 ms, p99 **70 ms** | < 100 ms | index-backed; 1M inserted in 169 s |
| cold start to ready | **~1.47 s** to base ready, ~2 s to harness ready | — | single-file binary, embedded-payload extract on first boot |
| idle RSS (one home) | ≈**305-310 MB** | — | 2×`beam.smp` (~144 MB each) + base ~6 MB + harness ~10 MB + oci container; a clean isolated read was impossible this pass (multiple bases live), so this matches REVIEW-P3's clean 309.2 MB rather than being re-measured in isolation |
| UI latency (bus event → UiModel frame) | ≤ **16 ms** (one coalesce frame) | — | the UI subscribes with `coalesce_ms:16`, so a durable/non-durable event renders within one batched frame by construction; not separately micro-instrumented (honest) |

Fairness: budgets are cleared by wide margins, but the bus figures are in-process against the Hub
(no wire), the query figures are against a single SQLite file, and the RSS/cold-start numbers carry
two `beam.smp` nodes (guardian + root) where a lighter single-node design would show less. Read them
as "well inside budget on this box," not as a cross-system benchmark.

## 7. Known gaps and deviations

The full deviation list is `rs/README.md` §"Deviations from the RFC" (70 numbered items). The ones
that bear on P4 specifically:

1. **P4.3 warm segments skipped** (§1) — deliberate, RFC-sanctioned; the query facade is the stable
   seam and the engine is P5 memory's. The single largest piece of the RFC plan not built.
2. **Split-payload secret leak** (§4.5, README dev. via P4.4) — a secret split across two envelopes
   is not masked; the fix is producer-side scrub, and the limit is documented and test-pinned.
3. **MCP streamable-HTTP is single request/response** — `POST /mcp` answers one JSON-RPC message per
   request; the SSE-streamed long-poll form of the MCP HTTP transport is not implemented (stdio is
   the streaming transport). Callable clients (Claude Code) work over stdio; the HTTP surface is the
   one-shot shape.
4. **Encoded-secret not caught** — the hub's leak scan is a literal-value substring match; a
   base64/hex-encoded copy of a secret value in a payload is not detected (same class as split-payload,
   same producer-side answer).
5. **doctor and guardian share names, not code** (README deviation 70) — a truly shared code list is
   impractical across the Elixir/Rust boundary (the guardian's probes are live frames to a running
   base, doctor is offline), so the convergence is the documented `@catalog` list doctor mirrors by
   name; the guardian loop is untouched.
6. **krun still unverified** (carried from P3) — no `/dev/kvm` on this box; compiled, unit tested,
   conformance self-skips with a reason string, which doctor's `sandbox` probe now surfaces verbatim.
7. **Adversarial suite is load-sensitive** (§2) — clean in isolation (20/20), can drop a
   guardian-reset timing test under heavy concurrent CPU; historically ~1/3 under load. Worth a CI
   note (run adversarial without a co-tenant build), not a code fix.

## 8. Risks

- **P4.3 is the biggest distance between "plan" and "shipped."** The warm-segment compactor is the
  one part of the RFC plan not built, so the 10M-event budgets of section 5 are unproven — the hot
  layer is validated at 1M (§6) and the query facade is stable, but nothing has scanned a
  Parquet/Tantivy segment because none exist. This is deliberate and P5-adjacent, but it is real
  scope not yet under test.
- **The leak guard is a substring matcher.** Split-payload and encoded-secret both slip it (§7.2,
  §7.4). The honest framing is that the hub scan is a backstop and the real answer is producer-side
  scrubbing; anyone treating `mask`/`block` as a complete DLP boundary is over-trusting it.
- **The security surface is wide and young.** Five real isolation defects were found by the
  adversarial suites in P4.4 alone (§4). The one default-deny scope guard is the right shape and now
  covers every method, but a listener this broad (https/WS/ingress/webhook/MCP) warrants continued
  adversarial attention as P5 adds consumers on top of it.
- **Two-node RSS and per-boot cost.** Every home carries a guardian and a root `beam.smp` (~144 MB
  each); a design that ran both roles in one node would roughly halve the idle memory. This is a
  deliberate isolation choice, not a defect, but it sets the floor for how light a single Tenon home
  can be.
- **Adversarial timing under co-tenancy** (§7.7) — the one place this pass saw red, and only under a
  concurrent build. CI should isolate the adversarial suite from other CPU-heavy jobs.

## 9. How to review in 15 minutes

Read in this order: `RFC-P4-plumbing.md` §0-3 (envelope + the four facades) and §9 (the P4 plan) for
intent; `rs/README.md` §"The plumbing" through §"`tenon doctor`" for the shipped shape; `NOTES.md`
§32-P4.8 for what each phase built and measured; this document for what an outside pass found.

Reproduce the e2e in ~10 minutes with the single-file binary:

```
export PATH="$HOME/.local/share/mise/shims:$HOME/.cargo/bin:$PATH"
source /home/ubuntu/workspace/deepseek.env.sh && export DEEPSEEK_API_KEY="$ANTHROPIC_AUTH_TOKEN"
# build the single-file http binary once (embedded payload):
cd beam && MIX_ENV=prod mix release --overwrite && cd ..
tar -czf /tmp/p.tgz -C beam/_build/prod/rel tenon_beam
cd rs && TENON_RELEASE_TAR=/tmp/p.tgz TENON_RELEASE_VERSION=0.1.0 cargo build --release --features http
BIN=target/release/tenon; export TENON_HOME=/tmp/review-p4
$BIN doctor                              # 7 probes, exit 0 once started
$BIN start && sleep 3
$BIN run "reply with the single word pong"
$BIN doctor                              # release + state_integrity now ok
$BIN stop --all
```

Rerun any single gate yourself, e.g.
`TENON_RELEASE_DIR=$PWD/../beam/_build/prod/rel/tenon_beam cargo test --release --features http --test ingress_gate`.
`AGENTS.md` has the per-subproject gate commands. A live demo base is left on `:38080`
(`TENON_HOME=~/.tenon-demo`) for direct inspection; do not stop it.
