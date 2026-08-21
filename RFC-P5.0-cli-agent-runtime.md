# RFC P5.0 — cli-agent runtime + safe self-evolution kickstart (draft)

Author: Fable, 2026-08-20. Status: draft for review (no code yet). Depends on P3 (runtime contract,
change protocol, sandbox, budgets, git-snap) and P4 (bus/kv/query/triggers). Goal: let an affordable
CLI agent (claude/agy `-p`) run long, explore, build/replace tools, and A/B them against the
unchangeable Rust+Erlang harness — WITHOUT risking the host's months of research code/docs, and
WITHOUT getting the Google/agy account banned. Small: ~<1k new LoC reusing existing pieces.

## 1. What it is (one paragraph)

A new runtime kind `cli-agent`: Tenon supplies the hands (sandboxed bash/fs/edit/git-snap, browse,
ingress) and the guardrails (snapshots, budgets, kill switch, event log); the CLI agent supplies the
brain. The agent proposes candidates (new tools, new harness components, new prompts); a deterministic
verifier on a benchmark set decides "better"; the change protocol promotes or rolls back. barebone
holds the judge and the kill switch. This is the first real user of P3's runtime contract + change
protocol + tools bus — not new architecture.

## 2. Why we can start now (memory/compaction not required)

The CLI agent brings its OWN context management: claude/agy `-p` each manage their own context window
and compaction internally. So the missing Engram/compaction (P5+) is exactly the gap the CLI agent
fills for the kickstart. Our record of the run is not a memory graph — it is the episodes table + the
per-step git snapshots we already have. Memory (P5) later mines those; it is not a prerequisite to
begin. Conclusion: yes, start now.

## 3. The three safety layers (host must survive a rogue agent)

Threat: agy runs on the host with the user's subscription creds and its own native tools; a bad step
could `rm -rf ~/workspace`, exfiltrate creds, or fork-bomb the box.

Layer A — confine the agent PROCESS on the host (non-negotiable floor, does not depend on the agent
cooperating). Wrap the `agy`/`claude -p` process in bubblewrap OR a Landlock+seccomp+rlimit jail:
- filesystem: RW only a throwaway `~/.tenon/cli/<run>/scratch`; RO the agent's own cred dir
  (`~/.config/<agent>` etc.) and minimal system paths; NO access to `~/workspace`, the tenon repo,
  `deepseek.env.sh`, `~/.ssh`, other homes. A rogue `rm -rf /` inside the jail destroys only scratch.
- process/resources: rlimits (NPROC, CPU, memory, open files) + a cgroup cap so a fork-bomb/OOM can't
  take the box down; `--die-with-parent` so killing the runtime kills the agent tree.
- network: allow only the agent's model endpoint (the credential host); deny the rest by default
  (bwrap netns + a tiny allowlist proxy, or accept unrestricted egress in v1 and document it).
- kill switch: base can SIGKILL the whole cgroup instantly.

Layer B — force the WORK into the Tenon sandbox (brain on host, hands in VM). The agent's real edits
and commands go through Tenon tools, not its native ones:
- preferred: connect the agent to Tenon-as-MCP-server (P4.7); disable/deny its native file/shell
  tools in the agent's config so its only tools are Tenon's sandboxed bash/fs/edit/snapshot.
- fallback (if the agent can't be told to use MCP only): a shim `$PATH` inside the jail replaces
  `bash`/`python`/`git` with thin forwarders to the Tenon worker; the scratch dir is the only real fs.
- every tool step is git-snapped; anything the agent does is reversible by `snapshot.restore`.

Layer C — the change protocol is the only path to permanence. The agent cannot mutate the barebone,
the Rust binary, or promote anything by itself; it calls `upgrade.propose`, and only a passing
verifier + benchmark gate promotes (P3.7). A bad candidate auto-rolls back. Host-affecting actions
(push-out, base config) need a human gate.

Net: even if the agent is fully compromised, blast radius = one scratch dir + one env's sandbox;
`~/workspace` and creds are unreachable at the kernel level.

## 4. Rate control (do not get the account banned)

The cheap "coding plan" subscriptions ban automated/batch use in their ToS; even API free tiers ban
on hammering. Mitigations, all in the runtime:
- token-bucket limiter: hard RPM well under the plan limit (config, default conservative e.g. 6 RPM),
  RPD cap, single concurrency per account (no fan-out to one account, ever), jitter between calls.
- pacing: this runtime is a SLOW overnight explorer, not a fleet — deliberate delays between steps;
  a task runs at human-ish cadence, not a tight loop.
- circuit breaker: on repeated 429/403, back off exponentially, then stop and raise a `violation`
  event + human alert; never retry-storm.
- one account = one serialized runtime; parallel exploration uses DIFFERENT providers/accounts
  (deepseek API, cerebras free) not the same subscription.
- budgets (P3.5) already cap wall time / calls / processes and hard-stop; wire the account limiter to
  the budget so overspend halts the env.
- honesty: keep usage patterns interactive-ish and low-rate; the CLI agent is for "long slow real
  work", the high-rate压测 fleet uses deepseek/cerebras APIs (no ToS ban) — different tools, per RFC
  model tiering.

## 5. Judging "better" (environment judges, never the AI)

- deterministic verifiers only: tests pass/fail, compiles, lint, benchmark-task success rate, and the
  cost (wall time, calls, steps) recorded in episodes. "Better" = same-or-higher success on the
  benchmark set at lower cost/steps than LKG. No self-evaluation.
- A/B: two envs, same task, candidate vs LKG, each in its own CoW workspace, compare the metrics over
  a distribution of tasks, not one run.
- subjective domains (elegance): not judged now; weak computable proxies only (tests green + fewer
  LoC + fewer deps + lower complexity), explicitly labeled "not quality". Human review at sprint
  boundaries keeps the barebone's human gate.
- benchmark set is the core asset: start with 5-10 deterministically-scorable tasks (fix a failing
  test, add a small feature with a provided test, refactor keeping tests green with fewer lines).

## 6. Scope and LoC

| Piece | ~LoC | Reuse |
|---|---|---|
| `runtime/cli-agent` adapter (spawn `agy`/`claude -p`, parse stdout->bus events, map its tool calls to Tenon tools / MCP) | 200-400 | runtime contract, tools bus, MCP server (P4.7) |
| host jail wrapper (bwrap or landlock+seccomp+rlimit+cgroup) | 150-250 | landlock backend (P3.1) |
| account rate limiter + circuit breaker | ~120 | budgets, bus violation events |
| benchmark runner + verifier scoring into episodes | ~200 | episodes table, change protocol |
Total < 1k new Rust, no new architecture, no kernel change.

## 7. Getting started (a real, minimal first run)

- P5.0a: host jail wrapper + `cli-agent` adapter that runs `claude -p`/`agy -p` confined to a scratch
  dir with Tenon-as-MCP as its only tools; every step git-snapped; budget + kill switch on. Gate: a
  rogue `rm -rf ~` inside the agent destroys only scratch; `~/workspace` untouched (tested).
- P5.0b: account rate limiter + circuit breaker; a soak test that the limiter never exceeds N RPM and
  stops on simulated 429s. Gate: measured RPM <= cap over a 100-call run.
- P5.0c: a 5-task benchmark set with deterministic verifiers; run the cli-agent overnight on it via a
  cheap model (deepseek API for the fleet copy) and via `claude -p` (subscription, slow) in a second
  env; collect episodes; nothing promoted without beating LKG. Gate: episodes accumulate; a candidate
  tool the agent writes is promoted only when it beats the benchmark, else auto-rolled back.
- First real target for the agent: "test my Rust+Erlang harness" — point it at the tenon test suites
  and let it find failures / propose worker-tool improvements, all sandboxed and snapshotted.

## 8. Open questions

1. Does `agy` support MCP + disabling native tools? If not, the `$PATH` shim (Layer B fallback) is
   required — verify before P5.0a.
   **RESOLVED (P5.0a, 2026-08-21).** `agy mcp add [flags] <name> <commandOrUrl>` registers an MCP
   server (stdio or http). The adapter writes a standard `mcpServers` `.mcp.json` and an
   `agy mcp add --header "Authorization: Bearer <token>" tenon <url>` register script, pointing agy at
   Tenon-as-MCP over loopback HTTP (serve `/mcp`). Disabling agy's native tools for the real run is the
   human step (`--sandbox` / config); the host jail is the safety floor regardless of tool config, so
   the `$PATH` shim fallback is not needed for the floor.
2. bwrap vs landlock+seccomp on this host (bwrap may need setuid; landlock is unprivileged) — pick the
   unprivileged path.
   **RESOLVED (P5.0a).** Unprivileged Landlock (ABI v2, `CompatLevel::BestEffort`, kernel 6.17 LSM) +
   `setrlimit` + best-effort cgroup v2. No bwrap. The rogue-`rm -rf ~` gate passes: a canary in
   `~/workspace` survives while scratch is the only writable tree. Seccomp was not added — Landlock +
   rlimit + cgroup is the v1 floor; a seccomp syscall filter is a later tightening.
3. Egress allowlist in v1 (proxy) or documented-unrestricted — lean documented-unrestricted first,
   tighten later.
   **RESOLVED for v1 (P5.0a): documented-unrestricted.** The agent needs its model endpoint and Tenon's
   MCP over loopback; the filesystem+rlimit floor is what protects the host. An allowlist proxy is a
   later tightening.
4. Which cheap model backs the "fleet copy" for the same benchmark (deepseek off-peak vs cerebras
   free) — decide per run.
5. **NEW (P5.0a): cgroup delegation.** cgroup v2 enforcement (`memory.max`/`pids.max`) requires base to
   run under the delegated `user@<uid>.service` manager; a process started from an interactive session
   scope cannot migrate into the delegated subtree (delegation-containment rule), so the adapter
   degrades to rlimit-only there (`RLIMIT_AS` stands in for `memory.max`). `RLIMIT_NPROC` must be set
   relative to the host's current per-uid process count, since it is per-uid, not per-tree.

## 9. Prereq check (2026-08-21, this host)

- `agy` and `claude` installed at ~/.local/bin. `agy` supports `--print`/`-p` (headless), `agy mcp`
  (manage MCP servers), `--sandbox` (its own terminal-restricted mode), `--dangerously-skip-permissions`.
  => Layer B preferred path works: register Tenon-as-MCP as agy's tool source.
- Host jail primitives: bwrap NOT installed, but Landlock is in the kernel LSM, unprivileged userns=1,
  cgroup v2 present. => Layer A uses Landlock + cgroup v2 + rlimit (unprivileged), reusing the P3.1
  landlock backend. No bwrap needed. No blockers.
- Decision: monitoring is TWO layers. Mechanical safety (Landlock jail + cgroup/rlimit + budget + kill
  switch) is always-on, microsecond reaction, does NOT depend on any LLM. LLM wake-checks are the
  JUDGMENT layer (progress/stuck/drift): Opus every ~10 min, Fable every ~1 h. Safety never relies on
  the LLM cadence.

## 10. P5.0c resolutions (2026-08-21, from the first real agy trial)

The CLI wiring (`tenon cli-agent run/preflight/status/stop`), the mandatory auth preflight, and the
scratch disk cap landed, and one supervised real `agy` trial ran. Findings that resolve or refine the
open questions above:

- **OQ1 refined — the auth probe.** A zero-cost preflight of `agy --version` + `agy mcp list` is NOT
  sufficient: neither authenticates, so a jail-blocked credential passes them and only fails on the real
  paid run ("You are not logged into Antigravity"). `agy models` DOES exercise the credential token
  source at zero cost and is the probe that catches it. The preflight now runs all three and scans for
  auth-failure signatures; only a clean pass allows a paid call.
- **NEW — credential dir must be READ-WRITE for agy, not read-only.** agy refreshes its OAuth token by
  writing to its own state dir, so a read-only grant breaks auth. The `--writable-state` opt-in grants
  the agent's own `~/.gemini` + `~/.cache` read-write (never `~/workspace`, the repo, `deepseek.env.sh`,
  or `~/.ssh` — the hard boundary is unchanged). Default stays read-only (the safe floor); a working agy
  run turns it on.
- **NEW — `RLIMIT_AS` is off for cli-agent runs.** agy/claude are Go/Node binaries whose runtimes
  reserve huge virtual address space; a tight `RLIMIT_AS` triggers a false `fatal error: out of memory`.
  Memory capping is delegated to the cgroup `memory.max`, enforced only when base runs under the
  delegated user manager (else no memory cap — documented). NPROC/CPU/NOFILE, the scratch watcher, and
  the wall/step budget remain the floor.
- **Tool routing for real work.** Under the jail with base down, agy's native terminal/file tool is
  degraded (`empty component: terminal_sandbox`) and did not land a file write, though the model turns
  ran. The intended path (RFC section 3 layer B) is base up + Tenon-as-MCP as the only tool source; the
  run registers it automatically when base is reachable.
- **Safety confirmed.** On every trial the `~/workspace` canary was byte-for-byte unchanged, nothing
  escaped scratch, and the run tore down cleanly — the kernel-level floor held regardless of the agent's
  auth/tool state.

## 10. v2 — sandbox-native design (supersedes the host-jail approach in §3, §6, §7)

The host-jail trial (2026-08-21) proved the safety floor works but exposed the real problem: agy's
native tools DEGRADE under a host Landlock jail ("empty component: terminal_sandbox") and agy is not
under our control, so forcing it through MCP is fragile. Decision: stop treating agy as "brain on
host, hands forwarded". Instead run agy INSIDE the env's OCI sandbox, exactly where the worker and
DeepSeek's tools already run — everything converges to one model: all agents run in the sandbox.

Correction to earlier notes: this host HAS containers (podman/docker work; the oci backend uses them);
it only lacks KVM (microVM/krun). No new VM is built — reuse the existing oci sandbox + bind mounts.

### 10.1 Mount model (RO base + RW overlay + persistent cache)

- creds/session: a persistent host dir/volume `~/.tenon/agy-session/` bind-mounted RW; the human logs
  in ONCE inside the sandbox (or drops the session file there); reused every run. The host's real
  `~/.gemini` is NOT mounted. `/etc/machine-id` mounted from a fixed per-sandbox value so agy does not
  see "a new machine" each run.
- workspace: a per-env RW dir the agent edits; the worker snapshots it (git-snap). agy uses its OWN
  native tools normally (a real container fs) — no MCP forcing, no tool forwarding; Tenon captures
  changes by snapshotting the workspace (on filesystem change via inotify, or a timer). Snapshot
  granularity is coarser than per-tool-call; the safety boundary is the container, so this is fine.
- read-only base: big prebuilt things (DSH installed on the host, toolchains) mount RO into the
  sandbox. The agent experiments freely; a break costs only a container recreate — RO base is intact.
- persistent cache: a host dir `~/.tenon/cache/<env>/` bind-mounted RW for node_modules/venv/pip/npm
  caches, with a small version manifest (what + version). On reload, reuse if versions match, else
  rebuild. Fixes the tmpfs 500 MB limit for ALL agents, not just agy. Benefits agy, DSH, big deps.

### 10.2 What this deletes vs adds

Delete: `rs/base/src/jail.rs` (host Landlock jail) and the host-jail path in cli_agent.rs; the
MCP-forcing complexity for agy. Reuse: oci sandbox + bind mounts + worker + snapshots (all built,
tested). Add: mount config (RO/RW/cache), the cred/session volume + one-time-login flow, a cache
version manifest, configurable sandbox machine identity. Net LoC ~flat or negative; conceptually one
layer instead of three (container boundary = safety; worker snapshot = rollback; no forwarding).

### 10.3 Isolation posture

Same boundary DSH/DeepSeek tools already use: the rootless-podman container. agy inside can touch only
its RW mounts (session volume, workspace, cache); the rest of the host is not mounted and is
unreachable. Restart is cheap (drop the container + workspace overlay; RO base + cache survive).
Residual risk = container escape, identical to the existing sandbox story, accepted and consistent.

### 10.4 Open risk to test first

agy's ToS / anti-automation may detect a container and refuse. MUST run a preflight INSIDE the
container (one zero-cost agy command) before any paid run; if agy hard-refuses in a container, report
and stop — do not work around it.

### 10.5 Configurable machine identity + mobile direction (roadmap only, ethics-bounded)

Make the sandbox's machine identity (machine-id, hostname, and later more env identity) a config
value. Direction (NOT built now, roadmap note): if the sandbox can host Android/iOS emulators, Tenon
gains mobile-app fullstack dev/test ability. ETHICS GUARDRAIL (hard rule): configurable identity and
emulators are for running the agent's OWN apps and legitimate cross-platform dev/testing ONLY — never
for bypassing the security controls of third-party services or any illegal use. No anti-detection /
jailbreak tooling is built. This guardrail is part of the barebone's hard rules.

### 10.6 Revised kickstart

- P5.0-v2a: refactor cli-agent to run the agent INSIDE the oci sandbox with the mount model; delete
  the host jail; add the cred/session volume + machine-id config; workspace snapshot on change/timer.
  Gate: with a fake agent, edits land in the sandbox workspace and are snapshotted; a canary in
  ~/workspace is unreachable (not mounted); container recreate is cheap.
- P5.0-v2b: cache mount + version manifest (node_modules/venv persistence); RO-base mount (DSH as the
  worked example). Gate: install a dep once, recreate the container, dep is reused from cache.
- Human step (once): log in to agy inside the session volume; then a container preflight confirms agy
  authenticates and can create a file in the sandbox workspace.
- Then the overnight benchmark run with monitors (mechanical always-on: container boundary + budget +
  kill + rate limiter; judgment: Opus ~10 min, Fable ~1 h).
