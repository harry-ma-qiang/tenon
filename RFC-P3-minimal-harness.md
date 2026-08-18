# RFC P3 — Minimal complete harness in Rust on the Tenon kernel (draft for review)

Author: Fable, 2026-08-18. Status: draft v3 (after discussion), for Gemini/human review. Numbering:
P{m}.{n} from here on (P0 toolchain, P1 kernel, P2 loader/sdk/bridge are done; this is P3).
v2 changes: one Rust binary; harness in Rust with the seam checklist; krun-only VM; change protocol,
blue/green kernels and LKG added; qemu-tcg dropped.
v3 changes: guardian + agent kernels as two BEAM nodes; VM by default with a single sparse workspace
image and step-granular git snapshots inside it; host state = one SQLite + blob dir; gix only inside
the worker; DSH plugins as the opt-in long tail; ~6k Rust target.

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
5. Two kernels, one implementation: base starts two BEAM nodes from the same payload. Node G
   (guardian): embedded mode, read-only code path, code loading disabled; runs the watchdog tree
   (health/budget/hard-rule probes over the wire, notifier, approval gate, `reset`). Node A (agent):
   the replaceable tree, agents may hot-load code there. Two nodes, not two kernels in one node,
   because a hot-loaded `tenon.erl` would replace the module for both. Reset = base kills A, restores
   LKG, restarts A; details of "crazy" detection are P3.5/P3.7, P3.0 only ships the two-node boot and
   the reset command.
6. Chicken and egg resolved: the base does not evolve itself. Humans ship base releases; agents evolve
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

Rough LoC (v3, minimal set only): base 1.2k, sandbox 0.8k, worker 1.5k, harness-lite 2k (loop, llm,
session log, tools bus, hooks; no compaction, no subagents, no UI), storage 0.5k = ~6k Rust + tests.
Everything else (compaction, subagents, web UI, skills, LSP) is the DSH long tail behind the bridge,
opt-in, not loaded by default. Reuse crates, no wheels: libkrun-sys, rusqlite (bundled), gix,
landlock, portable-pty, tokio, reqwest (streaming).

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

## 5. Filesystem: VM by default, one workspace image, snapshots inside it

- Default: the agent runs inside the VM with no host access. The only host writes are the config file
  and the state files in section 6. The VM's workspace is ONE sparse disk image (`workspace.img`,
  ext4, grows on demand, `fstrim` to shrink), attached as virtio-blk. Inside the guest it is a normal
  filesystem: native IO, nothing to notice.
- Model-facing tools are POSIX only (view/edit/write/bash + snapshot/time_travel). Single authority.
- Snapshots are step-granular, not per syscall: the worker commits the workspace tree with gix at each
  tool-step boundary (post-execute hook) and before risky operations; GIT_DIR lives inside the image.
  Rollback/diff/time-travel/CoW hypothesis workspaces are git operations inside the guest.
- Coherence and durability: single writer at a time (only the guest touches the image while the VM
  runs; the host never loop-mounts a live image). After each snapshot commit the worker fsyncs
  (`core.fsync=all` + `sync`; virtio-blk flush maps to host fsync), so a guest crash loses at most the
  in-flight step. The per-step packfile is pushed to the host over the wire and stored in
  `state.sqlite`; the guest git is a cache, the host copy is the truth for LKG and reset, so a
  destroyed image can be rebuilt.
- Growth control: expiry policy (keep last N steps, LKG, tagged, and one milestone every M steps;
  `gc --prune` the rest) plus periodic trim. No transparent git filesystem: none is production-proven
  in Rust and per-write commits would be far too slow.
- User's project files: default is clone-in / push-out with human review (strong isolation); opt-in
  virtio-fs mount of a host directory when convenience beats isolation.

## 6. Storage: SQLite + gix ODB, and how memory/navigator land on it

- Host state is three files: `config`, `state.sqlite`, `workspace.img`. No blob directories: SQLite
  is the application file format (BLOBs up to 1 GB per row, incremental read via `blob_open`,
  `incremental_vacuum` to shrink); large tool outputs and per-step snapshot packs are BLOB rows with a
  retention policy. gix is not used on the host.
- SQLite tables: `events` (session log, append-only), `tool_results` (index, blob hash), `snapshots`
  (step -> ref inside the image), `memory_nodes`, `memory_edges` (triples, confidence, outcomes),
  `embeddings` (blob; brute-force cosine at our scale; sqlite-vec later), `episodes` (state hash,
  action, verifier score, cost) for the navigator, `blobs` (sha256 -> bytes), `packs` (step -> git
  packfile). SQLite is not stored inside git and git is not used as a query engine.
- Rules: model-visible == logged (DSH law) applies to `events`; memory writes are commit-verified
  (explore in the working tree, commit only outcomes confirmed by verifiers); episodes are written by
  the loop for free from day one, so the navigator has data before it exists.
- Memory graph and navigator are P5/P6 plugins reading these tables; P3 only creates the schema and
  the write paths.

## 7. Plan (P3.x)

| Step | Deliverable | Test gate |
|---|---|---|
| P3.0 | Cargo workspace `tenon/rs/` (crates: base, worker, harness, storage, sandbox, sdk moved from sdk/rs); release layout `~/.tenon/`; base extracts the BEAM release and starts two nodes (guardian G read-only, agent A), `tenon start/attach/stop/reset` | base boots G + A from a profile; kill -9 base -> both nodes stop; `tenon reset` restarts A from LKG while G stays up |
| P3.1 | sandbox trait + `oci` + `landlock` backends; conformance suite | same worker tests pass on both; egress/deny policy enforced |
| P3.2 | worker: pty/fs/edit + step-granular git-snap inside `workspace.img`; folds playground spike + plugins/term; wire fd 3/4 and UDS/vsock; expiry policy | tool round trips, spill, PGID kill, snapshot/restore/expiry, 500 steps no leak, image grows then trims |
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
- Names: base, guardian node, agent node, worker, harness, memory graph, navigator, snapshot,
  workspace image, verifier.
- Two BEAM nodes (guardian G read-only, agent A writable) from one payload; VM by default; host state =
  config + state.sqlite + workspace.img (three files, no directories); gix only inside the worker;
  per-step packs pushed to the host; DSH long tail opt-in.

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
