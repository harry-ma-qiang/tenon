# RFC P3 — Minimal complete harness on the Tenon kernel (v4, consolidated)

Author: Fable. 2026-08-18. Status: v4.2 (built-in ASCII UI added) — in execution. Supersedes v1-v3.6 (history in
git). Numbering P{m}.{n}: P0 toolchain, P1 kernel, P2 loader/SDKs/bridge are done; this is P3.

## 0. Scope in one paragraph

Build the smallest set of pieces that makes Tenon a usable, self-improving agent harness: an
immutable, always-on barebone that can start, watch, and reset everything; replaceable runtimes
(environments) that agents may evolve at will, including spawning child environments; a sandbox by
default; a single-file host state; a change protocol with rollback; and a shipping shape of one Rust
binary that embeds the Erlang kernel. Everything not needed for that is either the DSH long tail
behind the bridge or a later phase.

## 1. Verdict on the claim "we demoed a minimal complete DSH in 1-2k Rust"

Right in spirit, wrong in size and in "complete". The playground spike proved: kernel + wire + Rust
tool suite + a loop + a memory file close the loop (~2-3k Rust) and the Rust SDK survives an outside
user. It lacked: event-sourced session log (replay/resume; model-visible == logged), policy/approval,
streaming + retry, structured events, persistence, and a base that survives the agent. The honest
minimal complete set is ~6.5-7k Rust + the existing 1k Erlang kernel + ~0.3k new BEAM code. Small,
not tiny.

## 2. Vocabulary

| Term | Meaning |
|---|---|
| barebone | base process + guardian node G + built-in kernel copy + built-in worker (LKG fallback). Human-owned, immutable at runtime, depth 0 of the environment tree. |
| base | the `tenon` binary in base role on the host: boots, watches, snapshots, resets, owns sandbox handles, the front-door UDS, LKG, kill switch. Runs no agent code. |
| guardian node G | BEAM node started by base in embedded mode, read-only code path, code loading disabled; runs the probe tree (health, budgets, violations), approval queue, `reset`. |
| environment (env) / runtime | one agent world: agent node A (BEAM) + harness + gateway + worker + one sandbox instance + one state file. Replaceable as a whole; there can be many; each has a parent. |
| agent node A | BEAM node per env running the loader tree for that env; agents may hot-load code there. |
| harness | `tenon harness` role, host process per env, holds the model key, runs loop/llm/session log/tools bus/policy hooks. Only tool calls cross into the sandbox. |
| gateway | plugin in node A listening on one configurable port (HTTP/WS/SSE; vsock/UDS same frames); each connection becomes a real fiber. How plugins born inside the sandbox register. |
| worker | `tenon worker` role inside the sandbox: pty/bash, fs, edit, git-snap; speaks wire. Built-in worker = LKG fallback; any wire-speaking worker may replace it. |
| sandbox | trait with backends krun (VM), oci, landlock. |
| snapshot / packs | per-step git commit of the workspace inside the sandbox; the packfile is pushed to the host state file (truth). |
| LKG | last known good: pinned {config, kernel beam, plugin manifest, profile, state copy}; `tenon rollback` restores it. |
| runtime contract | what any runtime must do to be supervised: register with its parent (manifest, health endpoint, event/approval channel), answer probes, write the event log. |

## 3. Architecture

```
host                                                    sandbox (per env)
+-----------------------------------------------+       +---------------------------+
| barebone: base (Rust) ---- guardian node G     |       |                           |
|   |  UDS front door, LKG, kill switch, spawn   |       |                           |
|   +-- env 1: node A -- harness -- gateway -----+-wire--+-> worker (pty/fs/edit/git)|
|   |          (loader tree, hooks)   port p1     |       |   + agent-born plugins    |
|   +-- env 2: node A -- harness -- gateway -----+-------+-> ...                     |
|   +-- env 1.1 (child of env 1) ...             |       +---------------------------+
| files: config, state.sqlite (+state-<env>.sqlite per env), [workspace-<env>.img optional]
+-----------------------------------------------+
```

Roles are subcommands of ONE Rust binary `tenon`: `start|attach|stop|reset|rollback|approve`
(base/CLI), `harness`, `worker`, `run "task"`. The BEAM release (kernel + loader + gateway + guardian
tree) is embedded as a payload and extracted to `~/.tenon/erts-<ver>/` on first run. Runtime is
several processes; the artifact is one file. Cargo workspace `tenon/rs/` with crates base, harness,
worker, storage, sandbox, wire (sdk/rs moves here); one bin target. TUI is P4.

## 4. Environment tree

- Only the barebone creates runtimes (as only a kernel forks). An env spawns a child by calling its
  parent's `runtime.spawn(overrides)`; each generation may override that method (changing behavior
  for its descendants) or delegate upward until the barebone performs the creation on the host
  (sandbox instance, node A, harness, gateway, state file). No nested VMs ever.
- The caller becomes the parent: the child is mounted as an external fiber in the parent's kernel
  tree (`tree` shows lineage, health/events flow up, child budget is a slice of the parent's); the
  barebone keeps a global registry. Config = parent profile + a patch layer (loader semantics).
- Children cannot touch parents: a child's RPC channel connects only to its parent (and to the
  barebone through the parent); socket ownership enforced; per-env OS privilege drop is P3.5.
- Pruning v1: parent death -> the barebone kills the whole subtree (fiber-cascade semantics).
  Reparenting to the grandparent is a later option.
- Limits enforced by the barebone: total sandbox count, RAM cap per env, depth (v1: 3).
- Any runtime meeting the runtime contract may replace the default one — an agent's own, or DSH
  mounted as the runtime — while the barebone keeps the old kernel watching, LKG and the kill switch.

## 5. Barebone

1. Boot: verify own hash + signed config; extract/start BEAM; start G; start the root env from the
   profile; expose the UDS front door; supervised by the OS service manager when installed (systemd
   --user / launchd), foreground in dev; base runs no user code, panic = abort.
2. Watch (G probes, core set fixed; extra probe plugins only from a signed/human-approved directory):
   env alive, kernel tree healthy, worker responsive, wedged waits, budgets, violations in the log.
   Health failures over N (configurable, default 6) or service gone -> reset (kill env, restore LKG,
   restart). "Crazy" heuristics beyond probes are later.
3. Snapshot and rollback: LKG per env and for the barebone config; `state.sqlite` copied at every
   LKG promotion; nothing from an agent-writable path is mounted without a snapshot first.
4. Hard rules v1 (inside the sandbox everything is allowed; rules live at the boundary): the sandbox
   boundary itself (no host access; keys stay on the host), budgets with hard stop (tokens, money,
   wall time, processes/sandboxes), kill switch (file/socket/signal), human gates only for
   host-affecting actions (workspace push-out, publishing, base config changes). No deny lists
   inside the VM.
5. Two nodes, one kernel implementation: G and A are separate BEAM nodes from the same payload,
   because a hot-loaded `tenon.erl` would replace the module for every kernel in one node.
6. Chicken and egg: the barebone does not evolve itself; humans ship base releases; agents evolve
   runtimes; the barebone rolls them back. Breaking the sandbox boundary is the only way past it.

## 6. Default runtime

- Harness (Rust, host, per env): llm adapter (DeepSeek/OpenAI-compatible, streaming, retry), agent
  loop (turn/step, tool calls), session log (append-only, replay/resume), tools bus (aggregate
  catalogs, single authority: DSH's overlapping tool rows are disabled when ours are mounted), policy
  hooks. Context overflow handling is deferred to the memory/navigator stage; v1 lets the model API
  error fail the turn gracefully.
- Seam checklist (every seam wire-exposed so any language can fill it): (1) system prompt assembled
  from registered sections; (2) tools registry add/replace/disable with priorities; (3) waterfall
  hooks pre-step, request, pre-execute, post-execute, turn-stopping; (4) llm as a service; (5)
  context injection hook; (6) delegation as a service; (7) verifier/scorer as a service; (8) the loop
  itself is a plugin (a new loop mounts beside the old, switches, old drains).
- Management tools for the agent (model-facing, with schemas): `plugin.list/mount/unmount/restart`,
  `config.get/patch` (patches are snapshotted and reloaded), `snapshot.list/restore`,
  `upgrade.propose/status`, `runtime.spawn`, `approval.request`; failures return the reason to the
  agent. A "how to extend Tenon" prompt section documents them.
- Gateway (BEAM plugin in node A, ~200 lines) + kernel socket-backed external fiber spec
  (`%{socket: S}`, ~80 lines Erlang): the in-sandbox registration path. One well-known, configurable
  gateway port (vsock in krun, TCP/UDS in oci/landlock; default 10000); every in-sandbox process
  (the worker included, in VM mode) connects there and each connection is one fiber — no channel
  multiplexing in the frames. fd 3/4 stays the path when the host spawns a plugin directly
  (oci/landlock, host-side plugins). Two ports, two processes (base UDS vs gateway); killing the
  sandbox or the gateway leaves base untouched.
- Worker: one resident async process per sandbox; pty sessions (ring buffer + spill), fs, edit,
  grep/glob and git-snap are in-process library calls, never a fork per tool call (the spike's
  `Command::new` per dispatch is not the model). Default built-in; replaceable by any wire-speaking
  worker (P3.7 formalizes).
- DSH long tail (compaction, subagents, skills, LSP, web UI) stays behind the bridge, opt-in.

## 6b. Built-in ASCII UI (the barebone's own face)

One dependency-free renderer, two carriers. `rs/ui`: `render(state, cols, rows) -> text`, pure
function over the event log, envs, kernel tree and approvals; pure ASCII (`+-|` borders), folding as
text markers. Responsive by columns: <80 one column (tree, transcript, events stacked), 80-140 two
(tree | transcript), >140 three (tree | transcript | event tail); rows size the tails. Terminal
carrier: `tenon attach --ui` (terminal size, ANSI clear + redraw on events; keys p prompt, a
approve, r rollback, q). Web carrier: optional `tenon serve --http 127.0.0.1:<port>` in base
(feature-gated axum): `GET /` returns a `<pre>` page (`?cols=`; works without JS; a few inline lines
may report viewport width and open SSE for refresh), `<form>` POSTs `/prompt`, `/approve/<id>`,
`/rollback`; CGI-like: every request renders once, no UI state on the server; localhost only. Served
by base, so it works when the runtime is broken — the guardian's window and the human gate UX. Does
not replace the P4 TUI or the DSH web app (still plugins). ~1-1.5k Rust.

## 7. Sandbox

```
trait Sandbox { spawn(image, workspace, policy, caps) -> Instance; attach via wire; destroy }
krun     libkrun (Linux KVM, macOS HVF; vsock; virtio-fs)   the only VM backend
oci      podman/docker; workspace volume; UDS back-connect   runs on this dev box
landlock process-level, Linux; the "--sandbox off" guard      runs on this dev box
```
Runtime detection (`/dev/kvm`, HVF) picks the default; config overrides. Same wire, same conformance
suite across backends. This dev box has no `/dev/kvm` (cloud ARM, cannot be enabled): develop on oci +
landlock here; validate krun on a Mac and on GitHub Actions Linux runners (KVM available). Windows
not planned until there is a machine. Egress allowlist mechanics per backend (TSI/oci) are an open
implementation detail.

## 8. Workspace and snapshots

- Guest root = read-only base image (alpine + py/node, OCI layers provided by the host, shared, not
  counted against the env) + tmpfs overlay as the workspace. Hard RAM cap per env (500 MB early);
  heavy toolchains live in the base image, never in tmpfs. The sandbox is disposable compute; the
  host log is the truth. Long-running guest processes die with a reset by design.
- Escape hatch: a workspace that outgrows RAM fails loud in v1; a sparse `workspace-<env>.img`
  (virtio-blk) is the opt-in alternative with the same tools and snapshots (single writer at a
  time; per-step fsync; host never loop-mounts a live image).
- Model-facing tools are POSIX only: view/edit/write/bash + snapshot/time_travel. Single authority.
- Snapshots are step-granular: the worker commits the workspace with gix at each tool-step boundary
  and before risky operations, respecting `.gitignore` from day one (ignored artifacts are not kept
  and must be rebuilt after a reset — the agent learns to be frugal). Rollback/diff/time-travel/CoW
  hypothesis workspaces are git operations inside the guest.
- Durability: after each step the worker pushes the packfile to the host; the host stores it in the
  env's state file and acknowledges. Guest git is a cache; the host copy is the truth for LKG and
  reset; a killed sandbox loses at most the in-flight step and is rebuilt by replaying packs.
- Growth control: keep last N steps, LKG, tagged, one milestone every M steps; `gc --prune` the
  rest; periodic trim. No transparent git filesystem (none production-proven; per-write commits are
  too slow).
- User's project files: default clone-in / push-out with human review; opt-in virtio-fs mount of a
  host directory when convenience beats isolation.

## 9. Storage and control plane

- Host files: `config`, `state.sqlite` (barebone) and `state-<env>.sqlite` per env; optional
  `workspace-<env>.img`. Every state file has one writer (its env's harness, or base for the barebone
  file); G and the CLI open read-only connections. Day-one pragmas: `journal_mode=WAL`,
  `synchronous=NORMAL`, `busy_timeout=5000`. No blob directories: SQLite is the application file format (BLOBs up to 1 GB,
  `blob_open`, `incremental_vacuum`); large tool outputs and per-step packs are BLOB rows with a
  retention policy. gix is used only inside the worker.
- Tables: `events` (append-only session log), `tool_results`, `snapshots` (step -> ref), `packs`
  (step -> packfile), `blobs` (sha256 -> bytes), `memory_nodes`, `memory_edges`, `embeddings`,
  `episodes` (state hash, action, verifier score, cost). Model-visible == logged applies to `events`;
  memory writes are commit-verified; episodes are written by the loop from day one so the navigator
  has data before it exists. Memory graph and navigator are P5/P6 plugins reading these tables.
- Versioning stance: `events` is the version history (state = fold(events)), `packs` the workspace
  history, LKG promotion copies the DB; SQLite session changesets if row-level DB time-travel is ever
  wanted. Why not DuckDB/redb/Kafka-style/Dolt-style: analytical vs append-heavy small rows, no
  SQL/FTS, not queryable, none mature in Rust respectively; DuckDB can read the SQLite file later.
- Control plane, three principles: (1) log = truth (`events` rowid is the offset; tail/replay come
  free; queryable); (2) kernel = live bus (emit/call in-VM ~1M/s, wire ~30k rt/s; the model is the
  bottleneck by three orders of magnitude; no ZeroMQ/iceoryx2 now); (3) one RPC schema, many
  transports (wire frames are the API; fd 3/4, vsock, base UDS JSON-RPC, gateway HTTP/WS/SSE,
  optional `tenon serve --http` are carriers). No bus component; tokio channels inside base;
  Kafka-class streaming only as a future plugin if thousands of agents make it measurable.

## 10. Change protocol and mutability tiers

| Tier | What | Who | Protocol |
|---|---|---|---|
| L0 barebone | base, G, built-in kernel/worker, hard rules, LKG | humans, signed releases | never at runtime |
| L1 kernel (node A) | `tenon.erl` in an env | agents | contract gate: pass `tenon check kernel` (the shipped contract suite; wire/API unchanged) -> canary -> hot load or blue/green; old beam kept; changing the contract needs a human |
| L2 plugins | harness, worker, gateway, tools, TUI, memory, navigator | agents | mount new -> conformance/health -> soak -> promote to LKG; failure -> auto rollback with reason |
| L3 config | yml/profile/prompt sections | agents | snapshot before every change |

One protocol, executed only by the barebone: `propose -> snapshot -> canary -> verify(contract +
rules + budgets + benchmark vs LKG) -> promote | rollback`. Human gates per tier are configurable (L0
humans only; L1 auto + notify by default; L2/L3 auto). Always online: fast path Erlang hot load,
safe path blue/green kernels (base starts N+1, health-checks, moves the front door, drains N; base
owns the socket). "Better" is measurable: contract suites + task metrics from `episodes`; promotion
only if it beats LKG on the benchmark set. The barebone holds LKG and the judge, never evolves.

## 11. Attach, detach, exit, replay, approvals

Approvals (two channels, no file sprawl): while attached the ASCII UI (or the plain CLI) prompts
inline; otherwise the pending request is a row in the `approvals` table, surfaced as a banner on the
next `tenon attach`, answered by `tenon approve <id>` or a UDS frame; G owns the queue and timeouts.

`tenon attach [env]` like vibe-term. Early setting `exit-on-detach`: exit -> every runtime stops
gracefully (drain, push packs, write log) -> start -> base rebuilds sandboxes, restores the latest
snapshot per env, harnesses resume sessions from the event log. Replay means restoring the latest
snapshot, not re-executing steps; long-running guest processes are not restored.

## 12. Plan P3.x

| Step | Deliverable | Gate |
|---|---|---|
| P3.0 | Cargo workspace `tenon/rs/`; `~/.tenon/` layout; base extracts BEAM and starts G + root env node A; `tenon start/attach/stop/reset`; UDS front door | boots from a profile; kill -9 base -> nodes stop; `reset` restarts A from LKG while G stays up |
| P3.1 | sandbox trait + oci + landlock; conformance suite; kernel socket-fiber + gateway plugin | worker tests pass on both backends; a python plugin started inside the sandbox registers through the gateway; killing sandbox/gateway leaves base untouched |
| P3.2 | worker as one resident process (in-process tools, pty ring buffers + spill, step git-snap, .gitignore, packs to host, expiry; registers via gateway in VM mode, fd 3/4 otherwise); `runtime.spawn` prototype (child env as external fiber, config = parent + patch, per-env state file, parent-death prunes, limits) | round trips, spill, PGID kill, snapshot/restore/expiry, 500 steps no leak; A spawns B, `tree` shows A->B, killing A removes B, B cannot reach A's RPC |
| P3.3 | harness (host, key) + seams + management tools + docs prompt section | real model turn; resume from log; guard denies; single authority; the agent mounts a plugin through the tools and sees it in `tree` |
| P3.4 | storage crate + schema; episodes written by the loop | replay a session from SQLite; episodes grow |
| P3.5 | built-in ASCII UI (`rs/ui`, `attach --ui`, `serve --http`, snapshot tests at 3 widths, HTTP GET/POST tests); hard rules v1, budgets, kill switch, approval RPC (`approval.request/answer`, `tenon approve`), runtime contract + `runtime.register`, probes, OS supervision, state copies at LKG, manifests, per-env privilege drop, exit-on-detach/replay | violation -> stop + rollback + notice; budget hard stop; kill -9 base -> supervisor restarts, A resumes from LKG; corrupted state replaced by LKG copy |
| P3.6 | krun backend (Mac/CI) + release CI producing the single `tenon` binary | krun passes the suite; fresh machine: download one file, `tenon run` works |
| P3.7 | change protocol + blue/green kernels; `tenon check kernel`; worker as replaceable plugin; benchmark gate | agent upgrades a plugin, a worker and the kernel without downtime; a bad upgrade auto-rolls back with the reason |
| P3.8 | simplify: zero-behavior-change refactor guided by `~/workspace/refactor.md` (typed RPC params via serde derive, shared `rs/test-support` crate for fixtures/polling, wire frame helpers) plus review findings; then full unit + integration + system tests, CLI and HTTP end-to-end | all suites green with 0 warnings before and after; LoC reduced 15-20% with tests kept; a written review for humans |

LoC: base 1.6k (incl. spawn), sandbox 0.8k, worker 1.5k, harness 2k, storage 0.5k, wire/sdk existing
= ~6.5k Rust + ~0.3k BEAM (gateway, socket-fiber, guardian tree). Reuse: libkrun-sys, rusqlite
(bundled), gix, landlock, portable-pty, tokio, reqwest. Order: safety floor (P3.0-3.1) -> what the
spike proved (P3.2) -> what makes it a harness (P3.3-3.4) -> hardening (P3.5) -> VM + release
(P3.6) -> evolution machinery (P3.7).

## 13. Readiness (what had to be true before starting; all now in the plan)

Plugins born inside the sandbox can register (gateway); the worker is replaceable (L2, built-in as
fallback); the agent has hands (management tools, config API, docs section); approvals exist; keys
never enter the sandbox; the barebone is OS-supervised and its truth is backed up at LKG; manifests
and `tenon check kernel` make rollback and kernel upgrades verifiable; environments form a supervised
tree with limits and pruning.

## 14. Open questions

1. Day-one hard-rule list and who may change it (signed config; hardware key later).
2. DSH bridge in the default profile or on demand.
3. Benchmark task set for the promotion gate.
4. Egress allowlist mechanics per backend.
(Approval UX: decided, section 11.)

## 15. Explicitly not doing now

Kafka/NATS/ZeroMQ/iceoryx2 bus components; gix FUSE or per-write commits; nested VMs; qemu-tcg;
Windows; context compaction (memory/navigator stage); deny lists inside the VM; per-plugin TS
fibers; single-file packaging beyond the embedded BEAM payload; reparenting of orphaned envs.
