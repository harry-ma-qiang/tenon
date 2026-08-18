# RFC P3 — Minimal complete harness in Rust on the Tenon kernel (draft for review)

Author: Fable, 2026-08-18. Status: draft v2 (after discussion), for Gemini/human review. Numbering:
P{m}.{n} from here on (P0 toolchain, P1 kernel, P2 loader/sdk/bridge are done; this is P3).
v2 changes: one Rust binary; harness in Rust with the seam checklist; krun-only VM; change protocol,
blue/green kernels and LKG added; qemu-tcg dropped.

## 1. Claim under review

"After the playground spike we can build a minimal, complete DSH-like harness runtime in Rust with a
few hundred to 2k LoC, like the kernel." Verdict: right in spirit, wrong in size and in the word
"complete". The spike proved: kernel + wire + Rust tool suite + a loop + a memory file close the loop
(~2-3k Rust) and the Rust SDK survives an outside user. It did not have: an event-sourced session log
(replay, resume, model-visible == logged), policy/approval, compaction, streaming + retry, structured
events for a UI, persistence, a base that survives the agent. The honest minimal complete set is the
spine below: ~5-7k Rust + the 1k Erlang kernel. Still small; not 1-2k.

## 2. The base (the always-on, non-replaceable part)

Purpose: the seed that starts life, watches it, and can reset it. Written by humans, changed only by
signed releases. Everything above it is replaceable by agents at runtime; the base is not.

Responsibilities (the `tenon` binary in base role + the Erlang kernel it launches):
1. Boot: verify own hash + signed config, extract/start the BEAM release (kernel + loader), mount the
   profile, start harness + worker, expose the CLI socket.
2. Watch: health of kernel and fibers, budgets (tokens, money, wall time, process/VM count), hard-rule
   violations, kill switch (file/socket/signal).
3. Snapshot and rollback: git-snap of plugin dirs + config + workspace per step; `refs/tenon/lkg`
   (last known good); `tenon rollback` restores LKG and restarts; the base never mounts code from an
   agent-writable path without a snapshot first.
4. Hard rules (few, simple, verifiable, enforced at the boundary, not inside the agent):
   - deny list: base binary/config paths, credential files, `rm -rf /`-class commands
   - egress allowlist (domains) for workers
   - budgets with hard stop
   - human gates for irreversible actions (git push, payments, deletes outside workspace, publishing)
   - immutable base: base runs outside the sandbox as a distinct OS user; agents run inside
     VM/container/landlock; the base holds the hypervisor/container handles
5. Chicken and egg resolved: the base does not evolve itself. Humans ship base releases; agents evolve
   plugins; the base rolls plugins back. A super-intelligent agent would have to break the sandbox
   boundary first; that is the ground the base holds "for a long while".

## 3. Minimal complete set (the spine)

| # | Piece | Language / where | Notes |
|---|---|---|---|
| S0 | base | `tenon` (base role), host | section 2 |
| S1 | kernel | Erlang (done) | registry, lifecycle, dispatch, wire, hot swap |
| S2 | loader + CLI | Elixir (done) | yml/patch tree, DSH profile writer; base wraps `tenon start` |
| S3 | harness | `tenon harness` role, Rust plugin process | llm adapter (DeepSeek/OpenAI-compatible, streaming, retry), agent loop (turn/step, tool calls), session log (append-only, replay/resume), tools bus (aggregate catalogs, single authority), policy/approval hooks, compaction stub |
| S4 | worker | `tenon worker` role (same file, inside sandbox) | pty/bash (tail + spill + PGID ladder), fs (view/edit/write/grep/glob), git-snap (snapshot/restore/diff), speaks wire over fd 3/4 or vsock/UDS |
| S5 | sandbox | Rust in base | trait + backends (section 4) |
| S6 | storage | Rust crate | SQLite (log/index/graph) + gix ODB (blobs/trees/snapshots) (section 6) |
| S7 | front door | CLI first; TUI = P4 | `tenon run "task"`, `tenon attach`, `tenon rollback` |
| S8 | DSH bridge | TS (done) | long tail of DSH stays available as one plugin |

Rough LoC: base 1.5-2k, harness 2-3k, worker 1.5-2k (folds playground spike + plugins/term), storage
0.5-1k, tests ~1x.

Shipping shape: ONE Rust binary `tenon` with four roles selected by subcommand — `tenon start|attach|
stop|rollback` (base), `tenon harness` (spawned by base as a plugin), `tenon worker` (the same file
executed inside the sandbox over fd 3/4 or vsock/UDS), `tenon run "task"` (CLI front door). The BEAM
release (kernel + loader) is embedded as a payload and extracted to `~/.tenon/erts-<ver>/` on first
run. At runtime there are still separate processes (base, BEAM, harness, workers); the artifact is one
file. One Cargo workspace (`tenon/rs/`) with crates base, harness, worker, storage, sandbox, wire/sdk;
one `bin` target. TUI later as a second binary or a feature.

## 4. Sandbox: one interface, three backends

```
trait Sandbox { spawn(image, workspace, policy) -> Worker; exec/attach via wire; snapshot; destroy }
backends: krun     (libkrun: Linux KVM, macOS HVF; vsock; virtio-fs)  <- the only VM backend
          oci      (podman/docker; workspace volume; UDS back-connect)  <- runs on this box
          landlock (process-level; Linux; the "--sandbox off" guard)    <- runs on this box
```
Runtime detection (`/dev/kvm`, HVF) picks the default; config overrides. Same wire, same conformance
tests across backends. This dev box has no `/dev/kvm` (cloud ARM, no nested virt, cannot be enabled):
develop against oci + landlock here; validate krun on a Mac (HVF) and in GitHub Actions Linux runners
(KVM available). Windows: not planned until there is a machine to test on. qemu-tcg dropped.

## 5. Filesystem: one model for the agent, two layers underneath

- Model-facing tools are POSIX only (view/edit/write/bash + snapshot/time_travel). Single authority.
- gix is the state layer under the POSIX tree: per-step snapshots (separate GIT_DIR, never the user's
  `.git`), CoW workspace derivation for parallel hypotheses (overlayfs/reflink where available, plain
  checkout otherwise), rollback, diff for the model, host<->worker sync (git push/fetch when strict
  isolation; virtio-fs when speed matters).
- No gix-backed FUSE filesystem now: large, slow, and compilers/tests need real POSIX anyway.

## 6. Storage: SQLite + gix ODB, and how memory/navigator land on it

- SQLite (rusqlite bundled, WAL): `events` (session log, append-only), `tool_results` (index, blob
  hash), `snapshots` (step -> ref), `memory_nodes`, `memory_edges` (triples, confidence, outcomes),
  `embeddings` (blob; brute-force cosine at our scale; sqlite-vec later), `episodes` (state hash,
  action, verifier score, cost) for the navigator.
- gix ODB: file blobs, trees, snapshots, large tool outputs (referenced by hash from SQLite).
- Rules: model-visible == logged (DSH law) applies to `events`; memory writes are commit-verified
  (explore in the working tree, commit only outcomes confirmed by verifiers); episodes are written by
  the loop for free from day one, so the navigator has data before it exists.
- Memory graph and navigator are P5/P6 plugins reading these tables; P3 only creates the schema and
  the write paths.

## 7. Plan (P3.x)

| Step | Deliverable | Test gate |
|---|---|---|
| P3.0 | Cargo workspace `tenon/rs/` (crates: base, worker, harness, storage, sandbox, sdk moved from sdk/rs); release layout `~/.tenon/`; base extracts + starts BEAM release, `tenon start/attach/stop` | base boots kernel + loader from a profile; kill -9 base -> kernel stops; restart resumes |
| P3.1 | sandbox trait + `oci` + `landlock` backends; conformance suite | same worker tests pass on both; egress/deny policy enforced |
| P3.2 | worker: pty/fs/edit + git-snap; folds playground spike + plugins/term; wire fd 3/4 and UDS | tool round trips, spill, PGID kill, snapshot/restore, 500 steps no leak |
| P3.3 | harness: llm + loop + session log + tools bus + policy hooks | real model turn; resume from log; guard denies; tools single authority (DSH rows disabled when ours mounted) |
| P3.4 | storage crate + schema; episodes written by loop | replay a session from SQLite; episodes count grows |
| P3.5 | hard rules + budgets + LKG rollback + kill switch | violation -> stop + rollback + human notice; budget hard stop |
| P3.6 | `krun` backend (Mac HVF / CI KVM) + release CI (Linux x64/arm64, macOS) producing the single `tenon` binary | krun passes the same conformance suite; fresh machine: download one file, `tenon run` works |
| P3.7 | change protocol + blue/green kernels (section 8b): `harness.upgrade`, `kernel.upgrade`, LKG promotion, benchmark gate | agent upgrades a plugin and the kernel without downtime; a bad upgrade auto-rolls back |

Order rationale: base and sandbox first (the safety floor), then the worker (what the spike proved),
then the harness (what makes it a harness), then storage, then the hard rules, then the microVM.

## 8. Decisions taken (v2)

- One Rust binary `tenon` (base + harness + worker + CLI roles) with the BEAM release embedded.
- Harness spine in Rust as a replaceable plugin process. Rationale: improvement happens by swapping
  plugins, not by editing loop code, provided the seams below exist. Erlang hot swap stays for the
  kernel; harness upgrades are restart + resume from the session log.
- Seam checklist (P3.3 acceptance; every seam is wire-exposed so any language can fill it):
  1. system prompt assembled from registered sections (prompt is data)
  2. tools registry: add/replace/disable with priorities (single authority)
  3. waterfall hooks: pre-step, request, pre-execute, post-execute, turn-stopping
  4. llm as a service (provider swap)
  5. context injection hook (memory, compaction, skills into the next request)
  6. delegation/subagent as a service
  7. verifier/scorer as a service (for tree search and memory)
  8. the loop itself is a plugin: a different loop mounts beside the old one, switches, old drains
- Loader/CLI stay Elixir (done, tested); base wraps them.
- Unix-only tool surface; gix as state layer.
- SQLite + gix ODB; no external DB.
- VM backend: krun only; oci + landlock as non-VM backends.
- Names: base, worker, harness, memory graph, navigator, snapshot, workspace, verifier.

## 8b. Change protocol, mutability tiers, always-online

| Tier | What | Who may change | Protocol |
|---|---|---|---|
| L0 base | Rust host: hard rules, budgets, snapshots, sandbox handles, kill switch, front-door socket, LKG | humans, signed releases | never at runtime |
| L1 kernel | `tenon.erl` | agents allowed | contract gate: new beam must pass the kernel contract suite (current tests + wire/API unchanged) -> canary -> hot load or blue/green; old beam kept for instant revert; changing the contract itself needs a human |
| L2 plugins | harness, worker, tools, TUI, memory, navigator | agents | mount new -> conformance/health -> soak -> promote to LKG; failure -> auto rollback |
| L3 config | yml/profile/prompt sections | agents | snapshot before every change |

One protocol, executed only by the base: `propose(change) -> snapshot -> canary -> verify(contract
tests + hard rules + budgets + benchmark tasks vs LKG) -> promote | rollback`. Agents call it as tools
(`harness.upgrade`, `kernel.upgrade`); human gates are configurable per tier (L0 humans only, L1
auto + notify by default, L2/L3 auto).

Always online: fast path = Erlang hot load (verified with live plugins); safe path = blue/green
kernels — the kernel already supports several instances per node; base starts kernel N+1 with the new
tree, health-checks it, moves the front-door socket, drains N. Base owns the socket, so a bad hot load
never takes the service down. Reset: `refs/tenon/lkg` pins {base config, kernel beam, plugin
manifest, profile}; `tenon rollback` = checkout + restart.

"Better" must be measurable: kernel contract suite, worker conformance, harness e2e smoke, plus
task-level metrics (success rate, cost) from the episodes table; a change is promoted only if it beats
LKG on the benchmark set. Base holds LKG and the judge; it does not take part in the evolution.

## 9. Open questions for review

1. (decided) harness in Rust with the seam checklist; kernel keeps Erlang hot swap.
2. (decided) base is a Rust process outside BEAM; it must outlive it.
3. Which hard rules are day-one, and who can change them (signed config; hardware key later)?
4. Human gate UX before the TUI exists: CLI prompt vs approval file vs web.
5. DSH bridge in the minimal set by default, or on demand?
6. Benchmark task set for the promotion gate: which repeatable tasks first?
