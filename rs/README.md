# rs — the `tenon` binary and the barebone

The host half of P3.0: one Cargo workspace, one binary, and the base process that boots and
watches the BEAM nodes. Base runs no plugin code, ever.

```
tenon start                      the base process
  |
  +-- run/base.sock  (UDS front door: nodes and CLI share it)
  |     ^                 ^
  |     |                 +-- tenon attach / status / stop / reset
  |     +-- guardian node G  --- health/reset -->  base
  |     +-- root env node A  <-- health/tree/reload
  |
  +-- state.sqlite   events (append only) + envs
  +-- lkg/           config + profiles + state copy, written at every good boot
```

| Crate | What |
|---|---|
| `base` | home layout, config, UDS RPC server, the node supervisor actor, LKG, release payload |
| `storage` | the state files: WAL, `synchronous=NORMAL`, `busy_timeout=5000`, versioned schema, `events`, `envs`, `packs`, `snapshots`, `tool_results`, `blobs`, `episodes`, `memory_*`, `embeddings`, `approvals`, retention |
| `sandbox` | the `Sandbox` trait plus `none`, `oci` (podman/docker) and `landlock` backends; `krun` is a P3.6 placeholder |
| `harness` | the `tenon harness` role (P3.3): the agent loop, the llm adapter, the session log, the tools bus and the management tools, one host process per env |
| `worker` | the `tenon worker` role (P3.2): one resident process inside the sandbox serving `bash`, `pty.*`, `fs.*` and `snap.*` over the wire |
| `cli` | the `tenon` bin target and `build.rs`, which embeds the BEAM release |

## Build

The binary needs a BEAM release to start nodes from. Three ways to give it one, in this
order of precedence: `--release-dir DIR`, `TENON_RELEASE_DIR`, the embedded payload.

```
cd ../beam && MIX_ENV=prod mix release        # -> beam/_build/prod/rel/tenon_beam
cd ../plugins/term && cargo build --release   # the demo plugin of the default profile
```

**Without payload** — the development shape, a 5 MB binary that needs a release directory:

```
cd rs && cargo build --release
TENON_RELEASE_DIR=$PWD/../beam/_build/prod/rel/tenon_beam ./target/release/tenon start
```

**With payload** — the shipping shape, one self-contained file. `build.rs` reads
`TENON_RELEASE_TAR` at build time and `include_bytes!`s it; unset, it builds without a
payload and `PAYLOAD` is `None`. `TENON_RELEASE_VERSION` (default `0.1.0`) names the
extracted directory:

```
tar -czf /tmp/tenon_beam.tar.gz -C ../beam/_build/prod/rel tenon_beam
TENON_RELEASE_TAR=/tmp/tenon_beam.tar.gz TENON_RELEASE_VERSION=0.1.0 cargo build --release
./target/release/tenon start        # extracts to ~/.tenon/erts/<version>-<sha>/ on first run
```

The extracted directory is keyed by version plus the first 6 bytes of the payload's sha256,
so a rebuilt payload lands beside the old one instead of overwriting a release a node may be
running from. An existing directory with a `bin/tenon_beam` is reused, so extraction happens
once. Measured here: 21 MB tarball, 68 MB binary, 1.2 s from `start` to both nodes up.

## Layout of `~/.tenon`

`--home DIR` or `TENON_HOME` moves it; every test uses its own.

| Path | What |
|---|---|
| `config.yml` | written with defaults on first start (see below) |
| `state.sqlite` | the host state file, one writer: base |
| `profiles/root/tenon.yml` | the default profile: one demo external plugin |
| `profiles/root/registry.yml` | the `name => spec` rows the loader resolves against |
| `plugins/<name>@<version>/manifest.json` | installed plugin versions: the loader's manifest registry source, pinned by the LKG manifest |
| `probes/` | extra guardian probe executables; only the ones `probes.extra` lists with a matching sha256 are ever run |
| `profiles/guardian/` | an empty entry list; G mounts only barebone plugins |
| `erts/<version>-<sha>/` | the extracted BEAM release, read-only to base and to the nodes |
| `run/base.sock` | the front door |
| `run/rt-<env>.token` | that env's runtime token (mode 0600): what a runtime the human starts by hand presents to `runtime.register` |
| `run/base.lock` | an exclusive `flock`, held for the life of the base process; guards against a second `start` |
| `run/base.ready` | holds the base pid while it is up; how `tenon start` waits. Written atomically (temp file + rename) |
| `run/{base,guardian,root}.log` | stdout and stderr of base and of each node |
| `run/gw-<env>/gateway.sock` | that env's `TENON_GATEWAY` unix socket. One directory per env, because the oci backend bind-mounts the socket's **directory**: a shared `run/` would put base's front door and every sibling's gateway inside every sandbox |
| `state-<env>.sqlite` | that env's state file: the `events` session log its harness appends to, the `packs`/`snapshots` of workspace snapshots pulled off its worker, and the `tool_results`, `blobs`, `episodes` and `approvals` the loop records beside them (see "Storage and the control plane") |
| `profiles/<env>/harness.yml` | that env's harness overlay: provider, model, the *name* of the key variable, `max_steps`, `approval`. Written with defaults on first start, patched through `config.patch` |
| `config-snapshots/<env>/harness-<ms>.yml` | the copy `config.patch` takes before every change |
| `run/harness-<env>.log` | stdout and stderr of that env's harness process |
| `envs/<env>/workspace/` | that env's sandbox workspace, bind-mounted at `/workspace` (oci) or granted read-write at the same path (landlock). `.tenon-snap/` (the snapshot GIT_DIR), `.tenon-out/` (handles and spill files) and `.tenon-restore/` (packs staged for a restore) live inside it |
| `profiles/<child>/overlay.patch.yml` | a spawned child env's own patch layer; its `TENON_PROFILE` is the parent's layers plus this file |
| `lkg/` | `config.yml`, `profiles/`, `state.sqlite` copied at every successful boot, plus `manifest.json`: the hashes `tenon rollback` verifies |

## Double start, boot signals, state integrity, node auth

Four properties an adversarial suite (`cli/tests/adversarial/`) checks beyond the happy path:

- **One base per home.** `tenon start` takes an exclusive `flock` on `run/base.lock` before
  touching anything else. A second `start` against a live home fails fast: it reads the pid
  out of the lock file, prints `already running (pid N)` and exits non-zero without removing
  the first base's socket or ready file. A lock left by a crashed base has no holder, so the
  OS releases it the moment the crashed process is gone; the next `start` takes it over and
  removes the stale `run/base.sock` and `run/base.ready` before doing anything else.
- **SIGTERM/SIGINT during boot cleans up.** Signal handlers are installed before the guardian
  or the root env is spawned, not after `base.ready` is written. A signal that lands while
  base is still waiting on `node.register` kills whatever nodes are already up (SIGTERM, a
  short grace, SIGKILL — shorter than `stop_grace_ms` since an unregistered node has no
  connections worth protecting) and removes `run/base.sock` and `run/base.ready` before the
  process exits.
- **`state.sqlite` is checked, not just copied.** At boot and at every `reset`, base runs
  `PRAGMA integrity_check` against `state.sqlite` (an unopenable or zero-length file counts as
  corrupt too). A healthy file is left alone — recent events are never discarded. A corrupt
  file is replaced from `lkg/state.sqlite` and a `state.restored` event is logged; without an
  LKG copy to fall back on (first boot) the file is simply removed so a fresh schema is
  created.
- **`node.register` needs a token.** Base generates a random 32-byte token per spawned node and
  passes it in `TENON_NODE_TOKEN`; the node's `node.register` must carry that token and the
  exact OS pid base recorded for that role/env, or base rejects it with an error frame and logs
  `node.register_rejected`. A registration from a plain CLI socket connection, which never has
  the token, always fails this check.

`config.yml`:

```yaml
root_env: root            # the environment started at boot
boot_timeout_ms: 30000    # both nodes must send node.register inside this
stop_grace_ms: 5000       # SIGTERM, then SIGKILL, per node
request_timeout_ms: 10000 # a health/tree/reload request to a node
max_restarts: 5           # unexpected deaths of one env before base gives up
sandbox: auto              # auto | oci | landlock | none
env_user: none            # the OS user an env's host-side processes run as
guardian:
  interval_ms: 2000
  failures: 6
  probe_timeout_ms: 5000   # a probe call slower than this is a wedge
probes:
  extra: []                # [{file, sha256}] under <home>/probes/
worker:
  boot_timeout_ms: 30000   # from "sandbox up" to the worker answering on the wire
  pull_interval_ms: 5000   # how often base pulls new snapshot packs off each worker
  keep_packs: 40           # packs kept per env in state-<env>.sqlite
envs:
  max_total: 8             # agent environments alive at once, the whole tree
  max_depth: 3             # root is depth 0, so a depth-4 spawn is refused
  ram_mb: 512              # per-env sandbox memory cap for spawned children
retention:
  keep_steps: 40           # newest snapshot steps state.retain keeps
  milestone_every: 10      # plus every Mth step, forever
  keep_events: 0           # 0 = keep the whole session log; N = keep the last N rows
  blob_grace_ms: 60000     # an unreferenced blob younger than this is left alone
```

The default profile mounts `plugins/term`'s release binary if it is built, otherwise
`playground/web/plugins/guard.py`, otherwise nothing. `TENON_DEMO_PLUGIN` names a binary
directly and `TENON_REPO` names the checkout; without either, base walks up from the working
directory looking for `kernel/src/tenon.erl`.

## Sandbox backends (P3.1)

`sandbox` in `config.yml` picks the isolation each **agent**-role env's instance runs
under (the guardian never gets one). `auto` (the default) probes in this order and uses
the first that works, `tenon_sandbox::detect()`:

1. **krun** — a P3.6 placeholder. Always reports unavailable here: `/dev/kvm absent` on
   this box, `krun backend arrives in P3.6` wherever `/dev/kvm` exists but the backend
   itself is not implemented yet.
2. **oci** — `podman` preferred, `docker` as a fallback, whichever is first found on
   `PATH`. Unavailable if neither binary exists.
3. **landlock** — Linux 5.13+, process-level. Unavailable if the running kernel has no
   Landlock support (probed with `CompatLevel::HardRequirement` against ABI v1).
4. **none** — always available, zero isolation; the base spawns no sandbox process at
   all and `sandbox.exec` errors for that env.

Explicitly naming `oci`, `landlock` or `krun` skips detection and fails boot immediately
with the same reason string if that backend is not usable — useful for a test fixture
that wants to assert a specific backend rather than whatever `auto` would pick.

Each env's instance is created when its node starts (`workspace` bind-mounted or granted
at `~/.tenon/envs/<env>/workspace`), destroyed and recreated on `reset`, and destroyed
when the env stops. Base runs no user code itself — it only spawns, execs into (via
`sandbox.exec`, below) and destroys instances, and reports their id/backend/attach
address in `status`.

**oci.** Default image `python:3.12-slim` (has `python3`, `sh`, `timeout`); override
with the `TENON_SANDBOX_IMAGE` env var on base. The image is **glibc**-based on
purpose since P3.2: base mounts its own binary read-only at `/usr/local/bin/tenon`
inside the instance and runs `tenon worker` from it, so the host binary has to load
there (Debian trixie ships glibc 2.41, this box builds against 2.39). `alpine` stays
a documented option and needs a musl static build of `tenon` — that is P3.6, not a
config switch. Memory capped at `policy.ram_mb`
(default 512, via `--memory`), process count at `policy.pids_max` (default 256, via
`--pids-limit`). The workspace directory is bind-mounted at `/workspace` inside the
container. For gateway reachability: a `unix:` `TENON_GATEWAY` has its **directory**
(not just the socket file, which may not exist yet) bind-mounted read-write at the same
absolute path inside the container and the env var passed through unchanged, so a
plugin connecting to `TENON_GATEWAY` from inside sees the same path as the host. That
directory is `run/gw-<env>/` and holds exactly one socket, which is what keeps a child
environment out of its parent's gateway and out of base's front door; a
`tcp:` gateway is passed through unchanged with `--network host` added (Linux-only,
documented here rather than attempted cross-platform). `TENON_SANDBOX_ENV` is a
comma-separated list of host env var names forwarded into the container (`-e NAME=value`
for each that is actually set on base). Every container carries three labels —
`tenon.env=<env>`, `tenon.home=<sha256(home)[..12]>` and `tenon.base=<base pid>` — and
its name embeds the home hash too, so two homes that both mount an env called `root`
(every adversarial test fixture does) never share a container identity; a leak from one
home's crashed base can never be mistaken for another's. `destroy` runs `stop -t 2` then
`rm -f`, tolerating an "already gone" container as success rather than an error; `Drop`
on the instance calls `destroy` again as a safety net (idempotent via an atomic flag) in
case something skipped the explicit call.

**Reap.** `Sandbox::reap(home_hash, all)` lists containers carrying that home's
`tenon.home` label and removes each one whose `tenon.base` pid is confirmed dead (via
`podman/docker inspect` + `kill(pid, 0)`), or every one of them regardless of liveness
when `all` is set; it returns the count removed. Base kicks this off once per boot, on a
`tokio::task::spawn_blocking` thread — never the actor's own task — right after the
sandbox backend is built and before the actor even starts processing `Cmd::Boot`; the
result reaches the actor later as `Cmd::SandboxReaped{count}`, logged as a
`sandbox.reaped` event. This is deliberately decoupled: an earlier attempt that ran the
equivalent `podman ps`/`rm -f` synchronously inside `enter_sandbox` (on the actor thread,
during `Cmd::Boot`) made the P3.0 `sigterm_during_boot_leaves_no_zombies` test flaky by
blocking the actor's single task behind a container-backlog round trip; a background
thread with the result delivered as an ordinary `Cmd` sidesteps that entirely. Humans get
the same operation via `tenon sandbox reap [--all]` (works whether or not base is up —
it opens the backend directly) and `tenon stop --all` (stop, then reap this home's dead
leftovers). See "What base does when something dies" below for why a reap is normally
unnecessary and only a `kill -9` of base needs it.

**landlock.** No persistent process: `spawn` just records the workspace and gateway
socket directory; `exec` restricts the *forked child* (via `Command::pre_exec`, before
`execve`, so the restriction is in place for the whole lifetime of the exec'd program)
to read-only `/usr /lib /lib64 /bin /sbin /etc /proc/self` and read-write the workspace
and the gateway socket directory, then runs the command directly on the host — the
"container path" and the host path are the same path. `destroy` is a no-op (nothing
persists). If the running kernel lacks Landlock, or only supports an ABI below v1,
`backend("landlock")` / `detect()` return the reason as an `Err(String)` rather than a
sandbox; the conformance suite and `tenon start` both surface that string instead of
crashing.

**Memory/pids caps and egress allowlisting are oci-only in P3.1** (`Policy.egress` is
carried through but not enforced by either real backend yet — RFC section 14, open
question 4). Landlock has no resource-limit concept; a future P3.5 privilege-drop pass
may pair it with cgroups for that.


## The worker (P3.2)

`tenon worker` is the resident tool process inside a sandbox instance. One process per
env, started by base once that env's node has registered (so its gateway is listening),
launched as `sh -c 'cd /workspace && TENON_GATEWAY=... nohup /usr/local/bin/tenon worker
--workspace /workspace &'` through `Sandbox::exec`. It dials `TENON_GATEWAY`, becomes a
fiber under that node's `gateway` fiber and provides the service `worker`. Nothing forks
per tool call: `bash` forks the user's command and that is the only fork.

Base polls `svc worker.info` until it answers, then reports the worker in `status`:

```json
{"env": "root", "registered": true, "worker": {"state": "ready", "pid": 41}, "sandbox": {...}}
```

`state` is `off`, `booting`, `ready` or `failed` (with the reason). A failed worker is an
event (`worker.failed`) and a status field, never a boot failure: the env still runs.

| Method | Arguments | Answer |
|---|---|---|
| `ping` / `info` | — | `"pong"` / `{pid, workspace, step, sessions, cap}` |
| `bash` | `{cmd, cwd?, timeout_ms?, env?, pty?}` | `{status, timed_out, bytes, tail, handle, truncated}` |
| `pty.open` | `{cmd?, cwd?, env?, cols?, rows?}` | `{session, pid, cols, rows}` |
| `pty.send` | `{session, data}` | `{session, bytes}` |
| `pty.read` | `{session, max?}` | `{session, data, bytes, dropped, alive}` |
| `pty.close` | `{session}` | `{session, status}` |
| `fs.view` | `{path, start?, end?}` | `{path, start, end, lines, total, content}` |
| `fs.write` | `{path, content}` | `{path, bytes, created}` |
| `fs.edit` | `{path, old, new}` | `{path, replaced, bytes}` or a loud error |
| `fs.grep` | `{pattern, path?}` | `{matches: [{path, line, text}], count, truncated}` |
| `fs.glob` | `{pattern}` | `{paths, count, truncated}` |
| `snap.commit` | `{label?}` | `{ref, step, label, files, at}` |
| `snap.list` | — | `{snapshots: [{ref, step, label, at}], count}` |
| `snap.restore` | `{ref}` (oid or step) | `{ref, step, files}` |
| `snap.diff` | `{a, b}` | `{a, b, files, insertions, deletions}` |
| `snap.pack` | `{since?}` | `{step, ref, bytes, refs, pack \| handle}` |
| `snap.apply` | `{packs: [{step, ref, handle}], ref?}` | `{applied, ref, step, files}` |
| `snap.expire` | `{keep_last?, milestone_every?}` | `{kept, removed, count}` |

- **Paths** are resolved under the workspace and normalised; an absolute path outside it
  or a `..` escape is an error, never a silent clamp.
- **`bash`** runs `bash -lc` (or `sh -c`) under a PTY by default (`pty: false` gives
  merged pipes). The child gets its own session/process group, so a timeout is a
  `killpg` ladder: SIGTERM, 500 ms, SIGKILL, reap; `status` is then `124` and
  `timed_out` is true. Output is streamed, never buffered whole: the last 32 KB come
  back as `tail` and, once the total passes that, the **full** output is written to a
  spill file in `.tenon-out/` and named by `handle` (so `handle`'s length equals
  `bytes`).
- **PTY sessions** are sentinel-free: a reader thread per session pumps the master fd
  into a 256 KB ring, `pty.read` drains it and reports how many bytes overflow dropped.
  No prompt detection, no marker injection — the caller polls. `close_all` runs on
  unload, so an unmounted worker leaves no child and no fd behind.
- **`fs.edit`** replaces a unique `old`. Zero matches and two matches are both errors
  that name the count and leave the file alone.
- **The wire cap is respected everywhere.** Anything whose JSON is bigger than
  `TENON_MAX_FRAME / 8` (clamped to 8 KB..256 KB) is written to `.tenon-out/` instead and
  answered as `{"handle": path, "bytes": n, "over_cap": true, "method": ...}`; `snap.pack`
  does the same with the raw packfile rather than base64 in the frame. Handles are always
  paths inside the workspace, so the host can read them straight off the bind mount.
- **`worker/step`** is emitted after every mutating call (`bash`, `fs.write`, `fs.edit`,
  the `pty` calls, `snap.commit/restore/apply/expire`) with `{step, method, ref}`.

## Snapshots, packs and reset

The worker commits the workspace with **libgit2** into a repository whose GIT_DIR is
`<workspace>/.tenon-snap` and whose work tree is the workspace itself — never the user's
own `.git`, which is excluded along with `.tenon-snap/` and `.tenon-out/` through the
repo's `info/exclude`. Staging is `index.add_all` + `update_all`, so `.gitignore` is
honoured from day one: ignored artifacts are not kept and must be rebuilt after a reset.

Each snapshot is a **parentless root commit** named by `refs/snaps/<step>`, with
`refs/heads/snap` on the newest. Parentless is deliberate: expiry becomes a ref delete
instead of a history rewrite, and every pack is self-contained, so replaying packs in any
prefix order still yields a checkoutable tree. `snap.expire` keeps the newest `keep_last`
plus every `milestone_every`-th step (and always whatever `refs/heads/snap` points at).

Durability is the RFC's: the guest repo is a cache, the host state file is the truth.

```
worker  --snap.pack{since}-->  base  -->  state-<env>.sqlite  packs(step, ref, bytes, created_at)
   ^                                          |
   +--- snap.apply{packs, ref} <--------------+   on reset: wipe workspace, write packs
                                                  into .tenon-restore/, check the newest ref out
```

- **Pulling is on a timer, not on the event.** Base runs one `snap.pull` per env every
  `worker.pull_interval_ms` (default 5 s). The alternative — reacting to each
  `worker/step` — would need the event to travel worker -> kernel -> a plugin -> Link ->
  base, which is more moving parts for the same result; a timer needs no plumbing, cannot
  wedge on a missed event, and coalesces a burst of steps into one pack. `snap.pull{env}`
  on the front door forces one immediately.
- **The acknowledgement is the stored step.** The next pull asks `snap.pack{since:
  max(step)}`, so the worker never resends a pack the host already has, and a pull that
  finds nothing new answers `{"pulled": 0}`.
- **`reset` replays.** Resetting an env kills the node, wipes the workspace, writes every
  stored pack to `.tenon-restore/<step>.pack`, starts the fresh instance and, once its
  worker answers, calls `snap.apply` — which folds the packs into a new `.tenon-snap` and
  checks the newest ref out. Committed files come back; uncommitted and ignored ones do
  not, by design.
- `snap.list{env}` on the front door shows what the host holds (step, ref, size, time).

## Storage and the control plane (P3.4)

One SQLite file per env (plus the barebone's own), one writer, every table of RFC section 9
in it. The harness never opens sqlite: it sends frames to base, which owns the file. `G` and
the CLI read the same rows.

| Table | Columns | Written by |
|---|---|---|
| `schema_version` | `version, at` | `Store::open`, forward only |
| `events` | `id, at, kind, env, data` | base (`emit`) and the harness (`events.append`); the session log, the version history |
| `envs` | `name, role, pid, status, at, parent, depth` | base, on every node state change |
| `packs` | `step, ref, bytes, created_at` | base, per `snap.pull` off that env's worker |
| `snapshots` | `step, ref, created_at` | base, with every pack: the same index without the payload |
| `tool_results` | `id, event_id, name, status, duration_ms, blob_hash, created_at` | the harness, one row per tool call |
| `blobs` | `sha256, bytes, size, created_at` | the harness (large tool outputs), deduplicated by hash |
| `episodes` | `id, session_id, step, state_hash, action, verifier_score, cost, created_at` | the harness, one row per step |
| `approvals` | `id, env, reason, status, created_at, decided_at` | base, on every `approval.request` |
| `memory_nodes` | `id, kind, text, confidence, outcomes, created_at, updated_at` | nobody yet (P5) |
| `memory_edges` | `src, dst, rel, confidence` | nobody yet (P5) |
| `embeddings` | `node_id, model, vector, dims` | nobody yet (P5) |

**Versioning is forward only.** `schema_version` holds the highest step applied; a file
written before it existed reports 0 and is walked through every step on the next `open`.
Every step is `create table if not exists`, so replaying step 1 over a P3.2 file only
stamps the row; the two columns P3.2 added to `envs` are still `alter table` attempts whose
duplicate-column error means "already there". `state.sqlite` and every `state-<env>.sqlite`
share one schema — the barebone uses three of the tables, an env uses all of them.

**Blobs are the escape hatch for size.** `put(bytes)` hashes with sha256 and inserts or
ignores, so the same output stored twice is one row; `get` reads the whole thing and
`open{offset, len}` is SQLite's `blob_open`, a window read that never materialises the row.
A tool output between 4 KB and 700 KB goes to a blob whole and the `tool_results` row
carries its hash, while the model keeps seeing the tools bus's cut view of it (8000
characters plus `[truncated]`) — the blob is for a reader that wants the rest, not for the
context window. The upper bound is the frame cap: base64 inflates by a third and a base
frame is 1 MiB, so a larger result keeps its `tool_results` row and loses only the blob. The
worker spills its own oversized outputs to files well below that, so nothing observed here
reaches the bound.

**Episodes are written by the loop from day one** so the navigator (P5/P6) has data before
it exists. One row per step, with:

- `state_hash` — 16 hex chars of `sha256(newest snapshot ref : id of the user message being
  answered)`. Base computes it, because base is what holds the workspace history.
- `action` — the tool calls of that step (`[{name, arguments}]`), or `"respond"`.
- `verifier_score` — **a placeholder**: 1.0 when every tool call of the step came back ok
  (a step that only answers counts as ok), 0.0 otherwise. A real verifier is P5/P6 work.
- `cost` — that step's token usage, as the llm adapter reported it.

**Retention is `state.retain{env}`**, running the `retention:` block of `config.yml` against
one env's file: keep the newest `keep_steps` snapshot steps, every `milestone_every`-th step
and whatever the newest ref (the LKG proxy) points at; drop the rest of `packs` and
`snapshots`; if `keep_events` is non-zero, keep only that many of the newest events and drop
the `tool_results` rows whose event is gone; then drop every blob nothing references any
more that is older than `blob_grace_ms`; then `pragma incremental_vacuum`. `keep_events` is
0 by default: the log is the version history, and a bounded file is a choice. `episodes` are
never pruned. New files are created with `auto_vacuum=INCREMENTAL`, so the vacuum gives
pages back without ever rewriting the file; a file created before P3.4 keeps whatever it was
created with and needs a full `vacuum` once for that to take.

```
harness                         base                       state-<env>.sqlite
  tool call --> events.append -----> events (id N)
            --> blobs.put ---------> blobs (sha256)          (only when > 4 KB)
            --> tool_results.append -> tool_results (event N, hash)
  step done --> episodes.append ---> episodes (state_hash, action, score, cost)
  reader    <-- episodes.tail{n} / blobs.get{hash,offset?,len?} / state.retain
```

## The environment tree (`runtime.spawn`)

`runtime.spawn{parent?, overrides}` is the P3.2 prototype of RFC section 4. A node asks
through `Link` (base takes the parent from the requesting connection) or a tool asks over
the front door with an explicit `parent`. Base — the only thing that creates a runtime —
builds the child on the host: its own sandbox instance, node A, `state-<child>.sqlite`,
workspace and gateway directory.

```
root  (depth 0)  profiles/root/tenon.yml
  +-- root.1 (depth 1)  TENON_PROFILE=profiles/root/tenon.yml:profiles/root.1/overlay.patch.yml
  |     +-- root.1.1 (depth 2)  ...:profiles/root.1/overlay.patch.yml:profiles/root.1.1/overlay.patch.yml
  +-- root.2 (depth 1)
```

- **Config is the parent's profile plus a patch layer.** `overrides.patch` is written to
  `profiles/<child>/overlay.patch.yml` and appended to the parent's `TENON_PROFILE`,
  which is now a `:`-separated list of loader layers; the loader applies a `.patch.yml`
  layer over the entry list exactly as its own patch semantics prescribe. `overrides.name`
  and `overrides.ram_mb` are the other two knobs (the RAM cap goes to the sandbox policy).
- **The child is a fiber in the parent's tree.** Base dials the parent's gateway and
  provides the service `env:<child>` (`status`, `stop`) on the child's behalf, so
  `tenon status` shows the child under the parent's `gateway` fiber, and `status`'s
  per-node `parent`, `depth` and `children` fields spell the lineage out.
- **Limits.** `envs.max_depth` (default 3, root being 0) and `envs.max_total` (default 8,
  guardian excluded) are checked before anything is created; both refuse with a reason.
- **Pruning is a cascade.** A parent that dies or is stopped takes its whole subtree with
  it: base halts every descendant deepest-first, destroys their instances, drops their
  `envs` rows and removes their state files, then restarts the parent alone.
  `runtime.stop{env}` does the same for one child on request (the root env refuses).
- **A child cannot reach its parent.** Each env's gateway socket lives in its own
  directory and only that directory is mounted into that env's instance, so from inside a
  child neither the parent's gateway nor `run/base.sock` exists at all; `node.register`
  still needs the per-node token base generated, so a stolen socket would not help either.

## The harness (P3.3)

`tenon harness` is the agent's own process: one per environment, on the host, holding the
model key. Base starts it once that env's worker has settled and hands it three things in
the environment — the gateway address to register on, base's front door to log through,
and the env's profile overlay as JSON. It is an ordinary wire plugin of that env's node,
so `tenon status` shows it as a fiber under `gateway` and unmounting the gateway drops it.

```
host                                            sandbox
  base <--- events.append, config.*, plugin.*, runtime.spawn, approval.request
   |                    |
   +-- spawn -->  tenon harness  --wire-->  gateway  --> node A kernel
   |                    |                                 |  svc worker.*      -> worker
   +-- state-<env>.sqlite  (the session log)               |  call tools/pre-execute
                                                           +--> guard.py, any language
```

Five services, all on the kernel bus, all reachable through `svc` from anywhere in that
node (and from the CLI over the front door):

| Service | Methods |
|---|---|
| `loop` | `session.create`, `session.prompt{session_id, text}` (queues, answers at once), `session.status`, `session.history`, `session.resume{session_id}`, `sessions` |
| `llm` | `chat{messages, tools?, stream?}`, `models` |
| `tools` | `register{name, schema, target:{service, method}, owner?, priority?}`, `unregister`, `list`, `execute{name, args}` |
| `prompt` | `register{name, order, text}` (returns the id that `unregister` disposes), `unregister{id \| name}`, `list`, `render` |
| `manage` | the management tools below, one method per operation |

**The llm adapter.** OpenAI-compatible chat completions with SSE streaming, tool calls and
token usage, configured per env from `profiles/<env>/harness.yml`:

```yaml
llm:
  provider: openai            # a label; the wire shape is OpenAI's either way
  base_url: https://api.deepseek.com
  model: deepseek-v4-flash
  api_key_env: DEEPSEEK_API_KEY   # the NAME of the variable, never the key
  temperature: 0
  timeout_ms: 120000
  retry_attempts: 3
  retry_base_ms: 200
max_steps: 8
approval: deny                # `auto` makes approval.request answer "approved"
tool_timeout_ms: 20000
```

Base reads that file, passes it to the harness as `TENON_HARNESS_CONFIG`, and forwards the
one variable `api_key_env` names if base itself has it. The key lives in the harness
process and nowhere else: not in the sandbox, not in the state file, not in an event.
`429`, any `5xx`, a timeout and a connection failure are retried with exponential backoff
(`retry_base_ms * 2^n`); a `4xx` is not. Streaming deltas are coalesced into
`assistant/chunk` events (240 characters or the end of the stream, whichever comes first).
Every request first runs the `llm/request` waterfall: an array back means "possibly
rewritten request", an object back short-circuits the model and *is* the answer.

**The session log.** Append-only `events` in `state-<env>.sqlite`, written through base
(`events.append`), never by the harness opening sqlite itself. Model-visible == logged:
`user/message`, `assistant/chunk`, `assistant/message`, `tool/call`, `tool/result`,
`turn/start`, `turn/end`, `step/start`, `step/end`, plus `session/created` and
`harness/ready`. Every row carries its `session`, so `session.history` is a filter and
`session.resume{session_id}` is a fold: user messages, assistant messages and tool results
in order become the model context again. That is what a restarted harness resumes from —
base restarts a harness that dies (bounded by `max_restarts`) and the sessions come back
on request, not automatically.

**The loop.** A turn is the queue entry for one prompt; a step is one model call. Each
step assembles the system prompt from the registered `prompt` sections in `order`,
collects the tool schemas from the bus, runs the `agent/pre-step` waterfall over
`{session, step, system, messages, tools}`, calls the model, logs the answer, and either
dispatches the tool calls (feeding each result back as a `tool` message) or asks the
`agent/turn-stopping` waterfall whether it may stop — a hook answering `{stop: false,
text}` keeps the turn going with one more user message. `max_steps` bounds it.
**Context overflow is not handled**: when the model API refuses an oversized context the
turn ends as `turn/end{ok: false, error}` and the session stays usable. The memory and
navigator stages own compaction (RFC section 6).

**The tools bus** is the single authority over model-facing tools. One name, one row; a
name registered twice by different owners keeps the higher priority and logs the loser
(`tools.register` answers `{ok: false, kept: <owner>, reason}`), while the same owner
replaces its own row. Every execution runs `tools/pre-execute` (hooks may rewrite the
arguments by passing the array on, or deny with `{deny: reason}`), then the target
service's method, then `tools/post-execute`. A denial and a failure are both tool results
the model reads, never a broken turn. The DSH rows that overlap ours are disabled in the
DSH profile when ours are mounted — a config note in the bridge's profile, not code here.

| Tool | Target | What |
|---|---|---|
| `bash` | `worker.bash` | a shell command in the workspace |
| `view_file` / `write_file` / `edit_file` | `worker.fs.view/write/edit` | the POSIX trio |
| `grep` / `glob` | `worker.fs.grep/glob` | search the workspace |
| `snapshot{op}` | `worker.snap.commit/list/restore` (list via base) | workspace time travel |
| `plugin{op, id, spec}` | `manage.plugin.*` | list/mount/unmount/restart the fibers of this node |
| `config{op, patch}` | `manage.config.*` | read or patch this env's overlay |
| `runtime_spawn{overrides}` | base `runtime.spawn` | a child environment |
| `approval_request{reason}` | base `approval.request` | ask a human |

The management tools are what make the environment editable from inside it, and the
"how to extend Tenon" prompt section (registered at boot, `order: 100`) tells the model
they exist. `plugin.mount` takes a registry-shaped spec (`{module}` or `{cmd, args, env}`)
and mounts it under the node's root fiber through `Link`; the mounted fiber shows up in
`tenon status` and in `plugin.list`. `config.patch` snapshots the overlay into
`config-snapshots/<env>/harness-<ms>.yml`, merges the patch (objects merge, everything
else replaces) and asks the node to reload its profile; the running harness keeps the
settings it started with, so a changed `llm` block takes effect at the next harness start
or `tenon reset`. `approval.request` is a P3.5 stub: it answers `denied` with the reason
`approvals not enabled` unless the env's overlay says `approval: auto`.

**Driving it.** `tenon run "task" [--env NAME]` creates a session, prompts it and streams
that session's events until `turn/end`, printing the assistant text as it arrives, the
tool calls and denials on stderr, and exiting non-zero if the turn failed. `tenon attach`
shows the same events for every session, mixed with base's own. Both read the log; nothing
about the loop is invisible.

```
$ tenon run "run echo tenon-ok with bash"
tenon run: tool bash {"cmd":"echo tenon-ok"}
tenon run: tool bash ok
the output was tenon-ok
tenon run: session s41-1 ok, usage {"completion":7,"prompt":11,"total":18}
```

## Hard rules v1: approvals, budgets, the kill switch (P3.5)

RFC section 5's three rules, all of them base's and none of them the agent's: a human gate in
front of host-affecting actions, budgets that stop rather than warn, and a kill switch with
three carriers. The guardian is told about each of them and owns none of them.

```
harness --approval.request--> base ---> approvals (barebone state.sqlite)  the queue
   |  (the call blocks)         |  ---> approvals (state-<env>.sqlite)     that env's history
   |                            +-----> notify{approval.pending} --> guardian node G
   |                            +-----> approval.pending event ----> tenon attach / run / UI
   +<--- {status: approved|denied|expired} <--- tenon approve <id> [--deny]
```

**The queue.** `approval.request{env, reason, kind}` resolves the env's mode first: `auto`
approves and `deny` refuses without a human, `ask` writes the pending row and holds the
caller. The mode comes from the env's overlay (`approval: ask` or `approval: {mode, timeout_s}`)
and falls back to `approval:` in `config.yml`. The row lands in **both** files: base's own
`state.sqlite` is the queue `tenon approvals` reads and the id `tenon approve` takes, the env's
`state-<env>.sqlite` is that env's own history. A verdict moves both and releases every call
blocked on it; `timeout_s` (60 by default) expires the row instead, and rows left pending by
an earlier base are swept to `expired` on the next request or list — nothing holds their
callers any more.

**The gates** are host-affecting actions only, per RFC section 5:

| Gate | When | Knob |
|---|---|---|
| `runtime.spawn` | the host already runs `spawn_soft_limit` environments or more | `approval.spawn_soft_limit` (2; `0` disables) |
| `config.patch{target: "base"}` | always — the barebone's config is L0 | — |
| `config.patch` of an env overlay | off by default: RFC section 10 makes L3 config agent-owned | `approval.gate_config_patch` (false) |
| `snap.export` | workspace push-out to a host path | `approval.gate_snap_export` (true) |
| any tool | the tool's name is in the profile's `gated_tools` | `gated_tools` in the env overlay, seeded from `approval.gated_tools` |

A gate resolves through **base's** `approval.mode`, never the env's overlay: an env may loosen
its own `approval.request` (the agent asking on its own behalf) but a child's patch layer must
not be a way past a host gate. A gated command holds its reply, asks the queue and — when the verdict
is `approved` — resumes as *the same command with its gate already passed*, so the gate is one `if` at the entry of the
actor and never a second code path. A refusal is an error the caller reads; for a gated tool it
is a tool result the model reads, so a denial costs a step, not a turn.

**Budgets.** `budgets: {tokens, usd, wall_s, processes}` per env, `0` meaning off, in
`config.yml` for the host and in the env's overlay for that env. Tokens come off the session log
itself: every `assistant/message` carries the usage the llm adapter reported, and base counts it
on its way into the state file, so the counter cannot disagree with what the model actually
cost. `usd` is that usage against `usd_per_1k: {input, output}` (or a per-provider table keyed by
the env's `llm.provider`). `wall_s` is time since that env booted, and `processes` is the
sandbox's own pid count, asked for on the `budget_tick_ms` timer and only when a limit is set —
it is a container round trip, not a read. A breach emits `budget.exceeded`, tells G, **halts
that env's harness** (the turn stops with it) and refuses every later `session.create` and
`session.prompt` with the reason. `tenon run` prints it and exits non-zero. `tenon reset` clears
the counters when `budget_reset_on_reset` is on (it is) and the env comes back.

**The kill switch** has the three carriers the RFC names: the file `<home>/run/STOP` (polled,
appearing and disappearing are the two edges), the frames `kill` and `resume`, and `SIGUSR1`.
Any of them halts every harness and refuses every prompt with the reason until the file is
removed or `resume` arrives, at which point the harnesses come back. `status` carries it as
`"killed": "<reason>" | null`, beside the per-env `"budget"` object.

```yaml
approval:
  mode: ask               # ask | auto | deny
  timeout_s: 60
  spawn_soft_limit: 2
  gate_config_patch: false
  gate_snap_export: true
  gated_tools: []
budgets:
  tokens: 0               # 0 is off, for every one of the four
  usd: 0.0
  wall_s: 0
  processes: 0
usd_per_1k:
  input: 0.0
  output: 0.0
budget_reset_on_reset: true
budget_tick_ms: 5000
```

## OS supervision and the per-env privilege drop (P3.5)

**The barebone under a service manager.** `deploy/` holds the two units (`deploy/README.md`
explains every line) and `tenon install-service --user` renders them for this binary and
this home: `$XDG_CONFIG_HOME/systemd/user/tenon.service` plus `systemctl --user
daemon-reload && enable` on Linux, `~/Library/LaunchAgents/com.tenon.base.plist` plus the
`launchctl` lines to run on macOS, and the unit printed with the two commands to run when
there is no service manager to talk to. `--print` writes nothing. It never starts base.
The unit runs `start --foreground` (the daemonising path would hide base behind a
`setsid` wrapper), `Restart=always` (nothing else restarts base) and `KillMode=mixed`, so
base's own ordered shutdown — flush packs, destroy instances, stop envs deepest-first,
then G — is not raced by a SIGTERM to the whole cgroup.

Base runs no user code: it spawns processes, owns sockets and files, and answers frames;
every agent-influenced thing is behind a process boundary. So the release profile sets
`panic = "abort"`: a panic in base is a bug humans shipped, and an abort the supervisor
restarts from is a better answer than a half-unwound actor still holding the front door.
`cargo test` builds the dev profile and still unwinds.

**`env_user`.** With `env_user: <name>` in `config.yml`, an env's *host-side* processes —
its node A and its harness — are `setgid`/`setuid`'d to that user in the forked child
before `execve`, and that env's own directories (`envs/<env>/`, `run/gw-<env>/`,
`profiles/<env>/`, `state-<env>.sqlite`, its two logs) are chowned to it. Base's own files,
the front door and the guardian node are never touched.

```yaml
env_user: none      # none (the default) | a user name
```

Resolution happens once per boot and is **best effort by design**: `none` is off, an
unknown user and a base that may not change uid (not root, no `CAP_SETUID`) both log one
line on stderr, emit `env.privilege` with `dropping: false` and the reason, and keep
running unprivileged. The alternative — refusing to boot an env that would otherwise be
supervised — trades a real barebone for a theoretical one. `env.chown` records the
handover per env when the drop is on. Only the unprivileged path is tested here (this box
has no root); the plan itself, the passwd lookup and the paths handed over are unit tests.

## Manifests, the LKG manifest and `tenon rollback` (P3.5)

Two manifests, one shape. A **plugin manifest** is what makes a version installable and
resolvable by name; the **LKG manifest** is what makes a rollback verifiable.

```
<home>/plugins/<name>@<version>/manifest.json      installed plugin versions
   {name, version, hash, cmd, args, protocol}      loader resolves profile names against these

<home>/lkg/manifest.json                           written at every LKG promotion
   {config_hash, profile_hash, release_version,
    plugins: [{name, version, hash}], state_copy: {path, sha256, bytes}}
```

- **The loader reads the plugin manifests** (`loader/README.md`, `Tenon.Loader.Manifest`):
  every node's profile gets `manifests: ["<home>/plugins"]`, so a profile row may name
  `echo` (whatever version is installed) or `echo@1.0.0` (pinned), and an explicit
  `registry.yml` row still wins over a manifest of the same name.
- **The LKG manifest is written by base at every promotion**, over the copies it has just
  taken: `config_hash` is the sha256 of `lkg/config.yml`, `profile_hash` one sha256 over
  the whole of `lkg/profiles/` (paths included, so a renamed profile moves it),
  `release_version` is the binary's version and the release directory it started nodes
  from, `plugins` is what was installed at that moment, and `state_copy` names and hashes
  the state file that was copied. It rides along in the `lkg.promote` event.
- **`tenon rollback [--force]` verifies before it restores.** Every hash is recomputed:
  the three LKG copies must still hash to what was pinned (a corrupt LKG must not be
  restored over a live home), and every pinned plugin must still be installed with the
  same hash. Any drift prints one line per difference — `what`, `pinned`, `found` — and
  refuses; `--force` restores anyway. It also refuses while base is up, because restoring
  `state.sqlite` under its only writer would corrupt exactly what is being rescued.
- **`tenon status --lkg`** prints the manifest plus `verified` and the same difference
  list, needs no running base, and exits non-zero when the verification fails — which
  makes it usable as a check in a script.

## Guardian probes (P3.5)

RFC section 5.2's watch, as a fixed set of probes in the guardian node plus the extra ones
base approved. The guardian owns no process and performs no reset: every probe is a frame
to base, and the verdict it can act on is one more frame, `reset{env, probes}`.

| Probe | What it asks | Fails when |
|---|---|---|
| `base` | `status` | base does not answer; the row it answers with is what the next three read |
| `env` | `health{env}` | the env's node does not answer `{ok: true}` |
| `tree` | `tree{env}` | the kernel tree is missing or its root fiber is not `active` |
| `worker` | `svc worker.ping` | base says that env's worker is `ready` and it does not answer `pong`, or base says `failed` |
| `harness` | `svc loop.ping` | the same for the harness's `loop` service |
| `wedged` | — | any probe call above took `probe_timeout_ms` or longer |
| `budgets` | the `base` row | that env's `budget.halted` is set |
| `violations` | `events.tail{env, after}` | a new event of kind `violation` or `budget.exceeded` is in that env's log |

- **A booting env is not a failing env.** The worker and harness probes read base's own
  view of the lifecycle first: `off` and `booting` owe no answer, `ready` owes a `pong`,
  `failed` is a failure. That is what keeps the guardian from resetting an env during the
  ten seconds its container takes to come up.
- **Violations are counted once.** The probe carries the id of the newest event it has
  seen and asks for what came after it, so one `budget.exceeded` row is one failing pass
  rather than a permanent one.
- `guardian.failures` consecutive passes with at least one failing probe (default 6) send
  `reset{env, probes}`; base emits **`guardian.reset`** with those names into that env's
  log before performing the reset. A pass with no failures clears the count.
- `guardian.probe_timeout_ms` (default 5000) is both the deadline of every probe call —
  the `link` service takes it per request now — and the line above which a call is a wedge.

**Extra probes are signed by being in base's config.** A probe plugin is an executable in
`<home>/probes/`, run with the env name as its only argument; a non-zero exit is a failing
probe named after the file. Base checks every entry of `probes.extra` before the guardian
node is started and passes only the survivors in `TENON_GUARDIAN_PROBES`:

```yaml
probes:
  extra:
    - file: disk.sh          # a plain name; no path, no ..
      sha256: 9f86d0818...   # must match the file, which must be executable
```

Anything else is a `probes.rejected` event naming the file and the reason (`sha256 is X,
the config says Y`, `is not executable`, `does not exist`, `no sha256 in the config`), and
the guardian never learns about it. Humans edit base's config; agents cannot, without a
gated `config.patch{target: "base"}`. Accepted probes are one `probes.loaded` event.

## The runtime contract (P3.5)

RFC section 2's last row, as an RPC. A runtime is one environment's world — node A, its
harness, its worker — and anything may replace it as long as base can supervise it. The
contract is what "supervisable" means, and `runtime.register` is where it is checked.

```
runtime  --runtime.register{env, token, manifest, health, channels}-->  base
                                                                          |
                                     token == run/rt-<env>.token ---------+
                                     contract: manifest{name,version,hash},
                                               health{kind: rpc|http, target},
                                               channels{events, approvals}
                                                                          |
              <-- probe: svc <service>.<method> | GET <url> ---------------+
                                                                          |
   {manifest, health, channels, probe_ms}  <-- recorded, `runtime.register` event
   error naming the reason                 <-- refused, `runtime.refused` event
```

- **Authentication is a per-env token**, generated with the env exactly like the node
  token, handed to that env's harness in `TENON_RUNTIME_TOKEN` and written to
  `run/rt-<env>.token` with mode 0600. A runtime a human starts by hand (DSH through the
  bridge — `bridge/dsh/README.md` has the frame) reads that file; the file permissions are
  the same protection `run/base.sock` has. A wrong token is `unauthorized` and nothing else
  is looked at.
- **The contract is checked before anything is probed**: all three objects must be there,
  every manifest field must be a non-empty string, `health.kind` must be `rpc` or `http`,
  and an `rpc` target must be a `service.method` pair. Each failure names the field.
- **Base probes the target the runtime declared**, rather than trusting the registration:
  an `rpc` target goes into that env's node as an ordinary `svc` frame, an `http` target is
  a plain GET that has to answer 2xx (hand-rolled, no client stack — the same stance as
  `serve --http`). The round trip time is kept as `probe_ms`.
- **Base registers the default runtime on behalf of its own env**, once that env's harness
  answers: manifest `tenon-default` at the binary's own version, hashed with sha256 over
  the running executable, health `rpc loop.ping`, channels `events.append` and
  `approval.request`. So `tenon status` shows a `runtime` object for every env from the
  first boot on, and a replacement is visible as a different manifest in the same field.
- **One row per env.** Registering again replaces it; starting or resetting an env drops
  it and the token with it. A refusal never disturbs the runtime already recorded.

## The built-in ASCII UI (P3.5)

`rs/ui` is the pure renderer (its own README covers the layout); base is what fills its model
and carries it. One model builder, `base/src/ui.rs`, reads four frames off the front door —
`status` for the tree and the budget line, `session.history` for the transcript, `events.tail`
for the tail, `approval.list` for the queue — and two carriers use it.

**Terminal.** `tenon attach --ui` puts stdin in raw mode with a `termios` guard that restores
it whatever ends the loop, reads keys on one blocking thread and redraws on three things: an
event from `subscribe`, a key, and a 400 ms tick that also re-probes the terminal size. Keys are
`p` (type a line, then `session.prompt` — creating the session on first use), `a` (approve the
first pending row after `y`/`n`), `r` (rollback: `reset` after `y`), `0`-`9` (fold or unfold that
transcript item) and `q`. Nothing is buffered on the host: every action is a frame.

**Web.** `tenon serve --http 127.0.0.1:<port>` behind the cargo feature `http` (off by default,
`cargo build --features http -p tenon-cli`). CGI-like: `GET /?cols=N` renders `html(model, cols)`
once, `POST /prompt`, `POST /approve/<id>` (`decision=approve|deny`) and `POST /rollback` act and
answer `303 See Other` back to `/`, so a reload never repeats an action and the server keeps no
UI state beyond one session id per process. Loopback only — the page is the human gate, not a
public surface — and it works with JavaScript off.

## Commands

| Command | What |
|---|---|
| `tenon start [--foreground] [--exit-on-detach] [--release-dir DIR] [--home DIR]` | boot G, boot the root env, open the front door. Without `--foreground` it re-execs itself detached (`setsid`), waits for `run/base.ready` and prints the pid |
| `tenon attach [--env NAME] [--ui]` | print the status document, then stream the event log until Ctrl-C. `--ui` renders the built-in ASCII UI instead (raw mode, keys `p a r 0-9 q`) |
| `tenon approvals [--status STATUS]` | the approval queue, one line per row: `id status env kind reason`. `pending` by default, `all` for the history |
| `tenon approve <id> [--deny] [--note TEXT]` | answer one pending approval; whatever call is blocked on it resumes or fails with the reason |
| `tenon serve --http ADDR [--env NAME]` | the same UI as a localhost web page (cargo feature `http`, off by default) |
| `tenon stop [--all]` | stop every env, then G, then base; `--all` also reaps this home's dead-base sandbox leftovers afterward |
| `tenon reset [--env NAME]` | SIGTERM/SIGKILL that env, restore its LKG profile, start it again. G is untouched |
| `tenon install-service --user [--print]` | write the OS service unit for this binary and home, and enable it where there is a user service manager; `--print` prints it instead. Never starts base |
| `tenon status [--lkg]` | one JSON document: base, both nodes, and each node's fiber tree. `--lkg` prints what the last promotion pinned and verifies it instead, without needing a running base, and exits non-zero when a hash moved |
| `tenon rollback [--force]` | restore the LKG config, profiles and state copy. Verifies every pinned hash first and refuses with what differs; `--force` overrides. Refuses while base is running |
| `tenon sandbox reap [--all]` | remove stale sandbox containers for this home; works whether or not base is running. Without `--all`, only containers whose `tenon.base` pid is confirmed dead go; with it, every container for this home goes regardless of liveness. A human-facing counterpart to the boot-time reap, for a home nobody is about to `start` again soon |
| `tenon run "task" [--env NAME] [--timeout SECONDS]` | one task for that env's agent: create a session, prompt it, stream the answer, exit 0 if the turn ended ok |
| `tenon harness [--env NAME]` | the agent process of one env. Base starts one per env; run by hand only against a live gateway |
| `tenon worker [--workspace DIR]` | the in-sandbox tool process. Speaks the wire on `TENON_GATEWAY` when it is set, fd 3/4 otherwise. `--workspace` defaults to `$TENON_WORKSPACE`, then `/workspace`, then the working directory |

`--exit-on-detach` stops everything when the last **subscriber** disconnects. `status` and
`stop` connect without subscribing, so only `attach` holds the door open.

**Detach, exit, replay.** The shutdown that follows the last detach is the same one
`stop` runs, and it is ordered: base asks every live worker for whatever it has committed
since the last stored pack and stores it (`base.flush`, 5 s per env), checkpoints every
state file, then destroys the instances and stops the envs deepest-first. The **next**
`start` treats the first boot of an env like a `reset`: the workspace is wiped, every
stored pack is staged into `.tenon-restore/` and the fresh worker folds them into a new
`.tenon-snap` and checks the newest ref out (`env.restored`). So a committed file comes
back inside a brand-new sandbox and an uncommitted one does not — replay is restoring the
latest snapshot, never re-executing steps (RFC section 11). The session log is untouched
by any of this, so `session.history` and `session.resume` answer for a session the
previous boot ran.

## RPC

Frames are the wire's: 4-byte big-endian length, then JSON, method in `t`, request/answer
correlated by `id`, `{"t":"rep","id":N,"result":...}` or `{"t":"rep","id":N,"error":"..."}`.
Ids are per direction. Nodes and CLI clients speak the same socket and the same frames.

| Method | From | What |
|---|---|---|
| `node.register{role,env,pid,token}` | node | the node is up; `token` must match the one base put in `TENON_NODE_TOKEN` for that role/env and `pid` must be the exact OS pid base spawned, or the request is rejected |
| `health{env}` | guardian, CLI | forwarded to that env's node |
| `tree{env}` | CLI | forwarded; the node's kernel tree |
| `reload{env}` | CLI | forwarded; `Tenon.Loader.reload/1` in that node |
| `reset{env,probes?}` | guardian, CLI | kill, restore LKG, restart. Refused for `guardian`. `probes` are the guardian's failing probe names and are logged as `guardian.reset` first |
| `svc{env,name,method,args}` | CLI | forwarded to that env's node as a `svc` frame; the node proxies it to `:tenon.svc/4` against its own kernel root ctx and answers with the plugin's result or error |
| `sandbox.exec{env,cmd,args,timeout}` | CLI | run `cmd args..` inside that env's sandbox instance (`timeout` ms, default 30000); answers `{status,stdout,stderr,timed_out}`. A test aid — the worker's tool surface is the agent-facing path |
| `snap.pull{env}` | CLI | ask that env's worker for everything committed since the last stored pack and store it; answers `{step, ref, bytes, pulled}`. Runs on a timer too |
| `snap.list{env}` | CLI | the packs the host holds for that env: `{count, packs: [{step, ref, bytes, created_at}]}` |
| `runtime.spawn{parent?,overrides}` | node, CLI | create a child environment; answers `{env, parent, depth, ram_mb, profile, service, pid}` |
| `runtime.stop{env}` | node, CLI | stop one child environment and its subtree; answers `{stopped: [...]}` |
| `sandbox.destroy{env}` | CLI | destroy that env's sandbox instance now, without touching the node; the next `reset` (or node restart) creates a fresh one. Also a P3.1 test aid |
| `plugin{env,op,plugin_id?,spec?}` | harness, CLI | forwarded to that env's node: `list`, `mount` (a `{module}` or `{cmd,args,env}` spec), `unmount`, `restart`. The fiber's id travels as `plugin_id` because `id` is the frame's own correlation id |
| `session.create{env}` / `session.prompt{env,session_id,text}` (both refused while that env is halted or the kill switch is on) / `session.status` / `session.history` / `session.resume` | CLI | forwarded to that env's harness as `svc{name: "loop"}`; how `tenon run` drives the agent |
| `events.append{env,kind,data}` | harness | one row in `state-<env>.sqlite`'s `events`, fanned out to every subscriber as an `{"t":"event","scope":"env"}` frame. Base is that file's only writer |
| `events.tail{env,after?,limit?}` | harness, CLI | that env's session log from `after` on; `env: "base"` reads the barebone's own log (boot, LKG, probes, sandbox) instead |
| `episodes.append{env,session_id,step,action,verifier_score?,cost,user_event?,state_hash?}` | harness | one `episodes` row; the state hash is computed here from the newest snapshot ref and `user_event` unless one is given |
| `episodes.tail{env,n?}` | CLI, plugins | the newest `n` episodes (default 200, capped at 5000), oldest first |
| `tool_results.append{env,event_id,name,status,duration_ms,blob_hash?}` | harness | one `tool_results` row against a `tool/result` event |
| `tool_results.tail{env,n?}` | CLI, plugins | the newest `n` tool result rows |
| `blobs.put{env,data}` | harness | store base64 `data`; answers `{hash, size}`, deduplicated by content |
| `blobs.get{env,hash,offset?,len?}` | harness, CLI | the blob as base64; with `offset`/`len` it is an incremental window read |
| `state.retain{env}` | CLI, plugins | run the `retention:` policy against that env's file; answers `{removed, left}` and emits `state.retain` |
| `config.get{env}` | harness, CLI | the env's harness overlay plus the paths it lives at |
| `config.patch{env,patch,target?}` | harness, CLI | snapshot `profiles/<env>/harness.yml` into `config-snapshots/<env>/`, merge the patch, ask the node to `reload`; answers `{snapshot, harness, reload}`. `target: "base"` patches the barebone's own `config.yml` instead — L0, always gated, and read at the next `start` |
| `runtime.register{env,token,manifest,health,channels}` | node, runtime, CLI | the runtime contract: authenticate with `run/rt-<env>.token`, check the manifest/health/channels shape, probe the declared health target, then record it. Answers the recorded document or an error naming the reason |
| `approval.request{env,reason,kind?}` | harness, CLI | ask for a human verdict. `auto`/`deny` answer at once; `ask` writes a pending row, tells G and **holds the answer** until `approval.answer` or the timeout |
| `approval.list{status?,limit?}` | CLI, plugins | the queue from the barebone's own state file; `status` defaults to every row, `all` is the same, `pending` is what `tenon approvals` asks for |
| `approval.answer{approval_id,decision,note?}` | CLI, the UI | `approve` or `deny` one pending row. The approval's id travels as `approval_id`: `id` is the frame's own correlation id |
| `snap.export{env,path}` | CLI, plugins | write that env's newest stored pack to a host path as a self-contained bundle. Workspace push-out, so it is gated |
| `kill{reason?}` / `resume{reason?}` | CLI, plugins | the kill switch over the socket: halt every harness and refuse every prompt, or let them back |
| `stop` | CLI | graceful shutdown of everything, base included |
| `status` | CLI | the snapshot plus one `tree` request per registered node |
| `subscribe{env}` | CLI | this connection starts receiving `{"t":"event",...}` frames; `env` keeps only that env's events plus the base-wide ones |

Requests to a node are answered outside the supervisor actor, so a `health` probe never
queues behind a `reset`. `reset` and `stop` do run inside it, for at most `stop_grace_ms`.
`sandbox.exec` and `sandbox.destroy` run the actual backend call in a `spawn_blocking`
task, not inside the actor, so a slow container exec never stalls `status`/`health`.

`status` reports the harness beside the worker: `"harness": {"state": "off" | "booting" |
"ready" | "failed", "pid": N, "restarts": N}`. A harness that dies is restarted while the
env keeps running (bounded by `max_restarts`); its sessions are in the log and come back
through `session.resume`.

`status`'s per-node `sandbox` field is now an object rather than a bare id string:
`{"backend":"oci","id":"tenon-6af3f8eda318-root-171...","attach":"unix:/home/x/.tenon/run/gateway-root.sock"}`,
or `null` for the guardian.

## What base does when something dies

| Event | Base |
|---|---|
| `kill -9` base | nothing — the nodes notice their socket closing and stop themselves (~1.1 s). Any sandbox instance base owned keeps running too; nothing survives a SIGKILL to call `destroy`. There is nothing to do about this *at* the moment of the kill — the next `tenon start` (or `tenon sandbox reap`) of the same home reaps it, since the leaked container still carries this boot's `tenon.base` pid and that pid is now provably dead |
| SIGTERM/SIGINT base | graceful `stop`: envs first (each env's sandbox instance is `destroy`ed as part of stopping it), then G, then exit. The RPC reply for `stop` (and `AbortBoot`, its during-boot equivalent) is held until this full teardown — env kill, sandbox destroy, all of it — actually completes, not sent the moment shutdown starts; a caller that trusts "ok" and force-kills base shortly after (a test fixture's teardown, an impatient supervisor) would otherwise race an in-flight `podman stop`/`rm -f` and orphan the container anyway, the same failure mode as the row above, just self-inflicted |
| the harness of an env dies | log `harness.exit`, start a fresh one against the same node (up to `max_restarts`); the env, its sandbox and its worker are untouched |
| an env node dies | log `node.exit`, restore its LKG profile, restart it, up to `max_restarts`. The old sandbox instance is `destroy`ed before the new one is spawned |
| G dies | the same, plus a loud line on stderr; the env keeps running |
| the guardian sees N failing probe passes | it sends `reset{env, probes}`; base logs `guardian.reset` with the failing probe names and performs the reset (old sandbox instance destroyed, new one spawned, same as an env restart) |

## Tests

```
cd ../beam && MIX_ENV=prod mix release        # the integration tests need it
cd ../plugins/term && cargo build --release   # for the demo-plugin assertion
cd ../../rs
cargo build --release && cargo build --release --features http -p tenon-cli
cargo clippy --all-targets --all-features -- -D warnings && cargo fmt --check
TENON_RELEASE_DIR=$PWD/../beam/_build/prod/rel/tenon_beam cargo test --all-features
```

`--all-features` is what compiles and runs the `http` carrier's own test; without it that one
test is not built and everything else is identical.

**Run `--test adversarial` on its own.** The three P3.5 gates are three more container-heavy
binaries, and `cargo test` runs test binaries in parallel: on this four-core box a whole-suite
run has up to six sandboxes starting and being torn down at once, and the adversarial suite's
15 s teardown assertions (`exit-on-detach` waits for base, its env's container included, to be
gone) start failing on load rather than on logic — seen twice in whole-suite runs here, green
every time the suite runs alone (`cargo test -p tenon-cli --test adversarial`, 195 s). The
numbers below are from the whole suite plus that separate adversarial run.

128 tests in the crates below (`cargo test --all-features` prints 152: `rs/ui` brings its own
24, covered by its README): `sandbox` unit 5, `boot.rs` 8, `storage` 14, `base` unit 16
(`token`, `home::hash` — stable per home, distinct across homes — the two `ui` model builders,
the http form decoder, the runtime contract's two, the probe approver, the three LKG-manifest
ones, the three privilege-plan ones and the two service-unit ones), the 20-test adversarial
suite, `sandbox`'s 2-test conformance suite, the 1-test gateway gate, the
P3.2 worker suites (`worker/tests/fs_test.rs` 9, `snap_test.rs` 9, `pty_test.rs` 10,
`cli/tests/worker_wire.rs` 3), the two P3.2 gates (`cli/tests/worker_boot.rs` 1,
`cli/tests/spawn_gate.rs` 1), the P3.3 suites (`harness/tests/llm_test.rs` 5,
`loop_test.rs` 12, `cli/tests/harness_gate.rs` 1, `harness_model.rs` 1), the P3.4 gate
(`cli/tests/storage_gate.rs` 1) and the seven P3.5 gates
(`cli/tests/approvals_gate.rs` 1, `budget_gate.rs` 2, `ui_gate.rs` 2, `contract_gate.rs` 1,
`guardian_gate.rs` 1, `manifest_gate.rs` 1, `replay_gate.rs` 1), which share
`cli/tests/gate/mod.rs` — one fixture (temp home, `config.yml`, `profiles/root/harness.yml`,
container reap on `Drop`) instead of seven copies of the one `harness_gate.rs` grew.

`cli/tests/contract_gate.rs` (1, needs only a release — `sandbox: none`, so no container and
7 s here) is the runtime-contract gate: base's own default runtime shows up in `status` with
a sha256 manifest and `loop.ping` as its health target, an outside runtime registers with the
env's `run/rt-root.token` against an `rpc` target and another against a real `http` health
endpoint (the DSH-through-the-bridge shape of `bridge/dsh/README.md`), and four refusals —
a 503 health endpoint, a forged token, a manifest without a version, an unknown health kind —
each come back with the reason, are logged as `runtime.refused`, and leave the last good
runtime in place.

`cli/tests/guardian_gate.rs` (1, skipped without oci or a release, ~23 s here) is the probe
gate: a boot whose `probes.extra` lists one script with the right sha256 and one with a wrong
one logs `probes.loaded{count: 1}` and `probes.rejected` naming the file and the hash it
found; then `SIGSTOP` on the harness makes the `harness` probe wedge, and within two probe
passes base logs `guardian.reset` carrying the failing probe names and the env comes back
with a fresh harness pid.

`cli/tests/manifest_gate.rs` (1, needs only a release, 3.5 s here) is the LKG-manifest gate:
a boot with a plugin installed under `plugins/echo@1.0.0/` writes `lkg/manifest.json` with
every field of RFC section 10 and the plugin pinned by hash, `tenon status --lkg` reports
`verified: true`, a plugin whose hash then moves makes `tenon rollback` refuse non-zero
naming the plugin (and `status --lkg` exit non-zero with `verified: false`), and with the
plugin back a broken live `config.yml` is restored from the LKG.

`cli/tests/replay_gate.rs` (1, skipped without oci or a release, ~13 s here) is the
exit-on-detach and replay gate, and the unprivileged half of `env_user`: a boot with
`env_user: nobody` logs `env.privilege{dropping: false}` with the reason and carries on; an
`attach` holds the door while a scripted turn writes and commits a file with the pull timer
turned off (`snap.list` is still empty); dropping that subscriber makes base push the pack,
flush and exit, socket and ready file gone; and the next `start` brings up a fresh sandbox
whose workspace has the committed file back and the uncommitted one gone (`env.restored`),
while `session.history` and `session.resume` still answer for the session the first boot
ran.

`cli/tests/approvals_gate.rs` (1, skipped without oci or a release, ~15 s here) is the P3.5
approvals gate: one boot whose profile lists `bash` under `gated_tools` and whose mode is `ask`
with an 8 s timeout, then four verdicts on real turns driven by the fake model. A `bash` call
blocks in the queue while `tenon approvals` lists it; `tenon approve <id> --note` releases it and
the tool result carries the sandbox's `tenon-ok`; a second call is answered `--deny` and reaches
the model as a denied tool result with the reason, the turn surviving; a third is answered by
nobody and expires, the row ending as `expired` and the model reading that; and a `snap.export`
to a host path — an RPC gate rather than a tool gate — is denied and refuses with
`snap.export needs a human`, leaving no file. Afterwards the pending queue is empty and
`approval.pending`, `approval.decided` and `approval.expired` are all in that env's log.

`cli/tests/budget_gate.rs` (2, same skips, ~11 s here) is the hard-stop half. The first drives a
token budget of 25 against a fake model that reports 18 per turn: the first turn passes and the
counter reads exactly 18, the second crosses the line, and then `budget.exceeded` names the
budget and the limit, `status` shows `budget.halted` on the env, the next `tenon run` is refused
non-zero with `halted ... budget tokens`, and `tenon reset --env root` clears the counter to 0
and the env answers again. The second is the kill switch: writing `<home>/run/STOP` makes
`status.killed` non-null within seconds and `tenon run` refuse with `kill switch`; removing the
file clears it, the harness comes back and a turn runs.

`cli/tests/ui_gate.rs` (2, same skips, ~19 s here) covers both carriers. `attach --ui` runs
under a real pty (`script -q -c ... /dev/null`), gets one `q` on stdin, exits, and its output
carries the ANSI clear, the env name, an ASCII border and the input hint. The HTTP test (only
built with `--features http`) starts `tenon serve --http 127.0.0.1:0`, reads the bound address
off stdout, and then: `GET /` is 200 with a `<pre>` page naming `root` and the prompt form,
`POST /prompt` is 303 and a `turn/end` shows up in the log, a pending `approval.request` raised
on another connection is resolved through `POST /approve/<id>` and the blocked caller sees
`approved`, and an unknown path is 404.

`storage` (14) is one test per table plus two the phase is about: `retain` over 100 packs
keeping exactly the newest five, every tenth and one LKG ref (and dropping the 85 others
with their snapshot rows), and a file built by hand in the pre-`schema_version` shape that
comes back migrated with its old rows intact and its new tables usable. The rest are round
trips: events in order, env upserts and the parent/depth tree, packs by step with their
snapshot index, blob dedup plus a windowed `open` past the end of the row, episodes queried
by session and as a tail, memory nodes/edges/embeddings (including the f32 round trip and
the cascade when a node is dropped), approvals from pending to a verdict and to expired, and
the day-one pragmas including `auto_vacuum=INCREMENTAL`.

`cli/tests/storage_gate.rs` (1, skipped without oci or a release) is the P3.4 gate: one
`tenon run` against a fake model whose scripted `bash` prints 20 000 characters, then four
assertions on what the loop recorded. `episodes.tail` shows one episode per step — step 1
with the `bash` action, step 2 with `"respond"`, both with the step's token cost and a
16-char state hash; the `tool_results` row for the call carries a `blob_hash`, `blobs.get`
returns the whole 20 KB and `blobs.get{offset, len}` the same bytes as a window, while the
`tool/result` event carries only the cut view; `session.history` matches a fold over
`events.tail` row for row; and after 100 recorded steps and a dozen real worker packs,
`state.retain` leaves exactly the pack steps the test computes from the policy itself, the
last 50 events, no blob whose tool result was pruned, and all 102 episodes. 17 s here.
`cli/tests/boot.rs` (8)
drives the real binary against a temp `TENON_HOME`: `tenon harness` without a
`TENON_BASE_SOCK` (exit 2, the reason on stderr), a `tenon
worker` whose `TENON_GATEWAY` names a socket nobody is listening on (exit 2, the connect
error on stderr — the worker never silently falls back to fd 3/4), a missing base, boot
(both nodes registered, the guardian tree carrying `guardian` and `link`, the root tree
carrying the demo plugin, the LKG written, and — since `sandbox` now defaults to `auto`
and this box has podman/docker — the root env's `sandbox.backend` is `oci`), `reset` (new
pid for A, unchanged pid for G, the old process gone), `kill -9` base (both nodes gone
inside 5 s), `stop` (base and both nodes gone, socket and ready file removed) and an
unexpected `SIGKILL` of A (base brings it back with `restarts: 1`). Without a release they
print a skip line naming how to build one and pass.

`harness/tests/` (13) needs no BEAM, no container and no key: `llm_test.rs` drives the
adapter against `harness::fake`, a tokio server that answers `/chat/completions` with
deliberately fragmented SSE — text arriving three characters at a time, a tool call whose
name and arguments come in three frames, `429` then `503` then success (three requests,
one answer), three failures in a row reported as one reason naming the attempt count, and
a `400` that is not retried. `loop_test.rs` drives the whole loop against doubles for the
bus and the log: a turn logging `session/created` through `turn/end` with the usage, a
tool call executed through the bus and fed back so the second request's roles are
`system, user, assistant, tool`, a `tools/pre-execute` hook denying with its own reason
(and the target service never called), a model `400` ending the turn as `ok: false` with
the session still usable, `session.resume` folding one session's rows back into three
messages while ignoring another session's, prompt sections rendering in `order` (and the
disposer removing one), the single-authority rules (a lower-priority owner is refused and
logged, a higher-priority one takes over), and a pre-execute hook rewriting the arguments
the target then receives.

`cli/tests/harness_gate.rs` (1, skipped without oci or a release) is the P3.3 gate, one
boot carrying five assertions: `tenon run "reply with the single word pong"` against a
fake model injected through `profiles/root/harness.yml` prints the answer and the log
holds `harness/ready` through `turn/end`; a scripted `bash` tool call runs in the sandbox
and comes back with `tenon-ok` in the tool result; the agent mounts a python plugin
through its own `plugin` tool and that plugin's service answers a `svc` call while its
fiber shows in the tree; a guard plugin started *inside* the sandbox through the gateway
denies an `rm -rf` tool call with its own reason, which reaches the model as the tool
result and the human as a line on stderr; and a `SIGKILL` of the harness is followed by a
fresh one, a `session.resume` that rebuilds the conversation and a next request that still
carries the first turn's text. 8 s here.

`cli/tests/harness_model.rs` (1) is the same first turn against the real DeepSeek endpoint,
skipped with a printed reason when `DEEPSEEK_API_KEY` is not set. The key is read by the
harness process from the variable the env's overlay names; it never enters the sandbox, the
state file or an event. 7 s here.

`sandbox/tests/conformance.rs` (2, `oci` and `landlock`; `krun`/`none` are excluded —
`krun` is not implemented and `none` runs no sandboxed exec by design) spawns, execs
`echo`, writes a file inside and reads it back from the host, runs a command past its
timeout and asserts it was killed, and asserts `destroy` leaves no container behind
(`oci`, filtered by **both** `tenon.env` and a per-run fabricated `tenon.home` label, and
with a nanosecond-suffixed env name — so a parallel `cargo test` run or a leftover from a
previous one can never make this assertion see someone else's container) or that a write
outside the workspace is denied (`landlock`, which has no persistent container to check).
`oci` additionally reads `/sys/fs/cgroup/memory.max` from inside and asserts it matches
the configured cap. Each backend prints `skipping <name>: <reason>` and returns (not
fails) if unavailable.

`cli/tests/gateway_gate.rs` (1) is the P3.1 gate itself: start base with `sandbox: oci`
(skipped if neither podman nor docker is on `PATH`); once the root env's sandbox instance
is up, copy `sdk/py/tenon.py` into its workspace, write a tiny plugin that `provide`s
service `inside` with method `ping`, launch it in the background inside the sandbox via
`sandbox.exec` (`nohup ... &`, connecting out to the node's `TENON_GATEWAY`); poll
`status` until a new fiber appears under the root tree's `gateway` node; call `svc{env:
root, name: inside, method: ping}` and assert `"pong"`; call `sandbox.destroy{env: root}`
and assert that fiber goes away (fails, or is unmounted) while `status` keeps answering;
`tenon reset` the env (the "restart" alternative to a Link `unmount{id}` request — it
tears down and remounts the gateway along with everything else in that node) and assert
`status` still answers afterward with the root node registered again.

`worker/tests/` (28) needs neither a release nor a container: `fs_test.rs` covers the
line-range view, the write/view round trip, a unique `edit`, `edit` failing loud on zero
and on two matches (file untouched), `grep` across nested directories skipping
`.gitignore`d files, `glob` on `**/*.rs`, and `../etc/passwd` plus an absolute outside
path both refused. `snap_test.rs` covers commit/list, `head`, an ignored file that is
neither snapshotted nor deleted by a restore, restore bringing old content back, a
tree-to-tree diff, expiry over twelve commits keeping the right set (and never dropping
what `refs/heads/snap` points at), and the **pack round trip**: `pack(None)` written to a
file, applied into a brand-new empty repo with `Snap::apply`, restored there, same bytes.
`pty_test.rs` covers `bash` under a PTY and under pipes, a non-zero exit, a timeout
killing the whole process group (the grandchild pid is gone afterwards), the spill file
whose length equals `bytes` while `tail` is exactly the trailing slice, a session
open/send/read/close with the pid gone afterwards, an unknown session failing loud, and an
fd count taken from `/proc/self/fd` around 50 session cycles and 200 `bash` calls.

`cli/tests/worker_wire.rs` (3) speaks the kernel half of the wire itself: it binds a unix
socket, starts the real binary as `tenon worker` with `TENON_GATEWAY` pointing at it,
answers `hello` with `load`, and then calls the service — `ping`, `info`, `bash`, `fs.*`,
`snap.commit/list/pack` and a live PTY session — over `svc` frames. It also runs the
frame-cap case (`TENON_MAX_FRAME=65536`, a 34 KB `fs.view` comes back as a handle inside
the workspace) and the **500-step loop**: 500 rounds of `fs.write` + `snap.commit` +
`snap.pack{since}` with `snap.expire` every 50, asserting every step lands, the snapshot
count stays bounded (<= 20) and the worker's fd count does not grow. 2.5 s here.

`cli/tests/worker_boot.rs` (1, skipped without oci or a release) is the P3.2 worker gate:
boot a home with `sandbox: oci` and `pull_interval_ms: 2000`, wait for `status` to report
the root env's `worker.state: ready` with a pid, run `bash` inside the sandbox through
`svc`, write and commit a file, wait for the **timer** to have pulled the pack into
`state-root.sqlite` (asserted through `snap.list`, no explicit pull), check a second
`snap.pull` returns `pulled: 0`, add an uncommitted file, `tenon reset`, and assert the
committed file is back in the fresh workspace with its content while the uncommitted one
is gone. 13 s here.

`cli/tests/spawn_gate.rs` (1, same skips) is the P3.2 tree gate, run against a home
configured with `max_total: 3` and `max_depth: 1` so the limits can be reached cheaply:
`runtime.spawn` from root yields `root.1` at depth 1 with an `overlay.patch.yml` layer,
`status` shows `children: ["root.1"]` on the parent and a new fiber under the parent's
`gateway`, a spawn from `root.1` is refused for depth, a third environment is accepted and
a fourth refused for the total, a `sandbox.exec` inside the child proves neither the
parent's gateway socket nor `run/base.sock` exists there (and a python `connect` to the
parent's gateway fails), and a `kill -9` of the parent node makes the child disappear from
`status` inside the grace while the parent itself comes back. 16 s here.

`cli/tests/adversarial/` (20, same skip-without-a-release rule) is the P3.0/P3.1 hardening
suite: double start refusal and survival of the first base, a crashed base's stale `run/`
files recovered by the next start, a five-round reset storm with no orphaned pids, `stop`
racing a `reset`, SIGTERM during boot leaving no zombie or orphaned BEAM process (stayed
green across the container-hygiene changes below — the boot-time reap never touches the
actor thread, so it cannot reintroduce the flakiness that kept it unwired before), twenty
parallel `status` calls during a `reset`, a corrupt `profile` and a corrupt `state.sqlite`
each restored from LKG on `reset`, guardian/env crash and restart-limit scenarios, a frozen
agent reset by the guardian without SIGCONT confusing base afterwards, two `attach`
subscribers and `--exit-on-detach`, RPC abuse (garbage bytes, an oversized frame header, an
unknown method, a half-open connection, a forged `node.register` from the CLI socket), and
`reap::a_leaked_container_with_a_dead_base_is_reaped_on_next_start`: hand-creates a
container carrying this home's `tenon.home` label and a pid from an already-reaped child
process (guaranteed dead) as its `tenon.base` label, then asserts a fresh `tenon start` of
that same home removes it while the real root env still comes up normally.

Every fixture across `boot.rs`, `gateway_gate.rs`, `worker_boot.rs`, `spawn_gate.rs` and
the adversarial suite also sweeps its
own home with `tenon sandbox reap --all` on teardown (`Drop`), independent of whatever the
test itself asserted — several of these tests deliberately `kill -9` base to test its
resilience, which leaks that boot's container by design (see "What base does when
something dies" above) and this home will never `start` again to trigger the ordinary
reap, so the fixture reaps it directly instead of leaving it for `podman ps -a` to
accumulate across CI runs.

## Deviations from the RFC

1. **No `rollback`, `approve` or `run` subcommand, and no `wire` crate.** They belong to
   P3.5/P3.7 and to the P3.1 move of `sdk/rs`; `sdk/rs` stays where it is for now.
2. **`sdk/rs` is the worker's wire.** It gained the `TENON_GATEWAY` socket transport in P3.2
   (`Plugin::try_new`, `wires()`, `connect()`; `Plugin::new` still exists and exits 1 with the
   reason when there is no wire at all), so the worker is an ordinary rust plugin that happens
   to run inside a sandbox. Base still speaks the same frames over its own socket rather than
   through the SDK.
3. **The guardian's release directory is not `chmod`ed.** G and A are started from the same
   extracted release, so a permission change would apply to both and would fight the payload
   extractor. Read-only is enforced instead by `RELEASE_MODE=embedded` and
   `RELEASE_DISTRIBUTION=none` in the release (no code loading, no remote shell, no epmd) and
   by base never writing under `erts/`.
4. **`reset` answers as soon as the old node is dead and the new one is spawned**, not when
   it has registered. That keeps the answer inside the requesting node's deadline and keeps
   the actor free; `status` reports `registered: false` until the node is back.
5. **The schema is complete and versioned.** `packs(step, ref, bytes, created_at)` arrived
   with P3.2 and lives in the **per-env** `state-<env>.sqlite`, not in the barebone's
   `state.sqlite`; `envs` gained `parent` and `depth` (migrated with `alter table` for homes
   written before P3.2). P3.4 added `schema_version` and the rest of RFC section 9:
   `tool_results`, `snapshots`, `blobs`, `episodes`, `approvals`, `memory_nodes`,
   `memory_edges`, `embeddings`. Both files get the same schema; which tables an env
   actually uses is what differs.
6. **The sandbox is on the boot path already.** Base resolves `config.sandbox` and spawns one
   instance per env through the trait, so P3.1 replaces a backend rather than adding a seam.
   The `none` backend hands back a `Direct` endpoint and is not isolation of any kind.
7. **`serde_yaml 0.9` is deprecated upstream** but is still the only mature serde YAML reader
   for Rust. Base only reads `config.yml` and writes two small profile files with it; the
   YAML dialect that matters is the loader's, on the BEAM.
8. **Daemon readiness is the `run/base.ready` file**, not an RPC handshake, so the detached
   `tenon start` needs no blocking frame client.
9. **The integration tests read `/proc/<pid>/stat`** to tell a live process from a zombie.
   That is Linux only, which this box is.
10. **`sandbox` defaults to `auto`, per the P3.1 plan**, which on this box means the default
    profile's root env now boots a real `oci` container instead of the `none` no-op — the
    P3.0 boot/adversarial suites still pass (42/42, run three times) but take noticeably
    longer (adversarial: ~105-190 s here depending on how many tests in the run stop a
    live sandboxed env, was near-instant with `none`) since every `reset`, node restart and
    `stop` spawns and tears down a container — `stop` now waits for that teardown to finish
    before answering (deviation 11's `Cmd::Stop` reordering), which is why it varied run to
    run more than the earlier `none`-backed number did.
11. **A `kill -9` of base still leaks that boot's oci container(s) — this is now handled,
    not merely documented.** Nothing survives a SIGKILL of base to call `destroy`, and
    `Drop` never runs across a killed process — the same tradeoff the RFC already accepts
    for base itself ("nothing — the nodes notice their socket closing and stop
    themselves"), just visible here as an idle `sleep infinity` container instead of a
    silently-reaped BEAM node. What changed in the P3.1 container-hygiene pass:
    `Sandbox::reap(home_hash, all)` now filters by a `tenon.home` label (in addition to
    `tenon.env`) and only removes a container once it has positively confirmed the
    `tenon.base` pid recorded on it is dead (`kill(pid, 0)`), never merely by env name —
    the earlier single-label scheme could not tell one home's `root` env container from
    another's, which is exactly what made `sandbox/tests/conformance.rs`'s own leak
    assertion flaky under parallel test homes. And reap now *is* wired into the boot path,
    just never on the actor thread: `spawn_reap` (in `base/src/lib.rs`) fires once per
    `foreground()` boot on a `tokio::task::spawn_blocking` thread, fully decoupled from the
    actor's `Cmd` queue, and reports back later as an ordinary `Cmd::SandboxReaped{count}`
    the actor processes whenever it gets to it. This is what made wiring it safe: the
    earlier attempt ran the equivalent `podman ps`/`rm -f` round trip synchronously inside
    `enter_sandbox`, which is called from `on_cmd` while handling `Cmd::Boot` — on the
    actor's own task — and blocked it long enough under container backlog to make
    `sigterm_during_boot_leaves_no_zombies` flaky. A background thread whose result arrives
    as a message sidesteps that structurally rather than by tuning a timeout.
    `podman/docker ps -a --filter label=tenon.home=<hash>` finds anything left behind for
    one home; `--filter label=tenon.home` (no value) finds it across all of them.
12. **`sandbox.exec`/`sandbox.destroy` are base RPCs, not CLI subcommands; `sandbox reap`
    is the one exception.** Nothing in `tenon`'s `clap` surface calls `sandbox.exec` or
    `sandbox.destroy` — they exist for `sandbox/tests/conformance.rs`, `cli/tests/gateway_gate.rs`
    and any other test or tool that already speaks the frame protocol via
    `tenon_base::client::Client`; P3.2's worker tools supersede them for agent-facing use.
    `tenon sandbox reap [--all]` breaks that pattern deliberately: it is a maintenance
    operation a human runs *because* base might not be reachable (a home whose base was
    `kill -9`'d and nobody has restarted), so it talks to the sandbox backend directly
    rather than through an RPC that presupposes a live base.

13. **The worker uses libgit2 (`git2`), not `gix`.** The RFC names gix. What P3.2 needs is
    `.gitignore`-aware staging of a whole work tree, a forced checkout that also removes
    files, tree-to-tree diff, a packfile builder and an odb pack writer. libgit2 has all
    five behind a stable API; gix has no index/add or checkout of that maturity in a
    released version and its pack-generation surface is low level enough to be its own
    project. `git2` is built with `default-features = false`, so the binary links only
    libz, libgcc and libc — which is what lets the host binary run inside the container.
    Revisit when gix's `status`/`add`/`checkout` land.
14. **Snapshots are parentless commits, not a chain.** `refs/snaps/<step>` each point at a
    root commit; `refs/heads/snap` names the newest. This makes `snap.expire` a ref delete
    instead of a history rewrite and makes every pack self-contained, at the cost of `git
    log` in the guest showing one commit per ref rather than a line of history. Diff and
    restore are tree operations and do not care.
15. **The worker is one resident process with reader threads, not a tokio runtime.** The
    plan said "one resident async process (tokio)". The wire SDK's loop is synchronous by
    design (`Rc` handlers, re-entrant `settle`), and PTY/child output is a blocking read on
    a raw fd; wrapping either in a runtime would buy nothing but a dependency, so the
    worker runs the wire loop on its main thread and one reader thread per PTY session.
    What the RFC actually asks for holds: one resident process, in-process tools, and no
    fork per tool call beyond the user's own command.
16. **Base opens the child's fiber in its parent's tree, not the child's `Link`.** RFC
    section 4 has the child's Link connect back to the parent's gateway as a plugin. Link
    speaks base frames (`node.register`, `health`, `tree`), not wire frames (`hello`,
    `provide`, `svc`), so making it do both is a BEAM change P3.2 does not need: base dials
    the parent's gateway itself and provides `env:<child>` on the child's behalf. The
    observable result — the child is a fiber under the parent's gateway, it dies with the
    parent, `tree` shows the lineage — is the same. The service is named `env:<child>`
    rather than `env` because a parent can have several children and service names are
    unique per kernel.
17. **`snap.apply` is a worker method the plan did not list.** It is the other half of
    `snap.pack`: base cannot fold packfiles into a repository itself (no git on the host
    side, by design), so the restore path hands the staged packs back to the fresh worker.
18. **`TENON_PROFILE` is now a `:`-separated list of loader layers.** `Tenon.Beam.Boot`
    splits it and merges a `registry.yml` from each layer's directory. That is the smallest
    BEAM change that gives a child env "parent profile + patch overlay" without base
    re-implementing the loader's patch semantics in Rust.
19. **The default oci image is `python:3.12-slim`, not alpine.** Base mounts its own
    (glibc, dynamically linked) binary into the instance to run `tenon worker`; musl alpine
    cannot load it. Alpine remains a documented option once P3.6 produces a static build.
20. **Packs are pulled on a timer (5 s, configurable), not on each `worker/step`.** Both
    were allowed; the timer needs no event plumbing from inside the sandbox to base, cannot
    wedge on a dropped event, and coalesces bursts. `snap.pull{env}` forces one.
21. **The P3.2 tree gate runs against lowered limits** (`max_total: 3`, `max_depth: 1`)
    rather than spawning four generations of real containers to trip the shipped defaults.
    The refusal path is identical; the test is 16 s instead of minutes.

22. **The harness has its own async wire client, not `sdk/rs`.** The SDK's loop is
    synchronous (`Rc` handlers, a re-entrant `settle`), which is right for the worker and
    wrong for a process whose calls are a model streaming for half a minute: a blocked read
    loop would mean no `session.status` during a turn and no second session at all.
    `harness/src/wire.rs` speaks the same frames over tokio — one writer channel, a pending
    map for `call`/`svc`, a task per inbound request. The worker keeps the SDK.
23. **`agent/turn-stopping` is a waterfall, not a `bail`.** The wire has no `bail` frame,
    so the veto is `{stop: false, text?}` from a `call`-mode hook. Same for `llm/request`,
    where an object back (rather than the argument array) short-circuits the model.
24. **The management tools are grouped by an `op`, not named with dots.** OpenAI-compatible
    tool names are `[a-zA-Z0-9_-]`, so the model sees `plugin{op}`, `config{op}` and
    `snapshot{op}` while the `manage` service still exposes `plugin.list`, `config.patch`,
    `snapshot.restore` and the rest as individually named methods for plugins.
25. **`config.patch` does not restart the running harness.** Restarting it mid-turn would
    drop the tool result the model is waiting for. The patch is snapshotted, written and
    followed by a loader `reload`; a changed `llm` block applies at the next harness start
    or `tenon reset`. `approval.request` is likewise a stub until P3.5 owns the queue.
26. **A plugin the kernel spawns now gets `TENON_GATEWAY` unset.** An agent node exports it
    so processes born inside the sandbox can dial in, but an SDK that prefers the gateway
    (`sdk/py` and `sdk/rs` both do since P3.2) would open a *second* fiber for a plugin the
    kernel had just spawned and leave the port-backed one waiting for a `hello` forever.
    `Tenon.Beam.Registry.spec/1` appends `{"TENON_GATEWAY", false}` to every spawned
    plugin's env; `Link` also answers `svc` and `plugin` requests from a spawned process now,
    so a minute-long tool call cannot block the guardian's health probes.

27. **Retention prunes the event log only when asked.** `keep_events` defaults to 0, so
    `state.retain` touches `packs`, `snapshots` and unreferenced `blobs` and leaves `events`
    alone. RFC section 9 makes `events` the version history and RFC section 8 only asks for
    growth control over the workspace history; a host that wants a bounded file sets the
    knob, and only then are the `tool_results` rows of dropped events (and the blobs they
    were the last reference to) dropped with them. `episodes` are never pruned at all: they
    are the navigator's training data and they are tiny.
28. **Blobs travel base64 over the front door.** The frame protocol is JSON, so `blobs.put`
    takes and `blobs.get` answers base64 rather than raw bytes; the harness encodes once per
    oversized tool output. `blobs.get{offset, len}` is the incremental read, so a reader that
    wants a 100 MB blob pages it rather than decoding it whole.
29. **Base computes `episodes.state_hash`, not the harness.** The hash folds the newest
    snapshot ref and the id of the user message being answered, and base is what holds both;
    computing it in the harness would cost a `snap.list` round trip per step. The harness may
    still send an explicit `state_hash` and base will store that instead.
30. **`verifier_score` is a documented placeholder.** 1.0 when every tool call of the step
    came back ok, 0.0 otherwise. It is not a judgement of whether the step helped; the real
    verifier is P5/P6 work and the column is here so the rows exist before it does.
31. **The seven P3.4 methods share one `Cmd::Records` variant.** `episodes.*`,
    `tool_results.*`, `blobs.*` and `state.retain` are all "one env's state file, one
    accessor, one JSON answer"; spelling each as its own `Cmd` would be seven identical
    shapes for no extra safety. The front door still names them individually.
32. **The model sees the head of a large tool result, not its tail.** The RFC's phrasing
    ("the model still sees the tail") is about the model keeping a bounded excerpt while the
    whole output goes to a blob. What the tools bus has cut since P3.3 is the head — the
    first 8000 characters plus `[truncated]` — and P3.4 did not change that, so a `bash`
    call's beginning is what reaches the context and `blobs.get{hash}` is what reaches the
    rest. Making the excerpt a tail (or both ends) is a one-line change in `Outcome::text`
    whenever a task shows it matters.
33. **`state.retain` runs on demand, not on a timer.** `keep_packs` already bounds `packs`
    per pull, so nothing grows unbounded between calls; the policy pass is a frame the CLI,
    a plugin or (from P3.7) the change protocol sends. A timer would be four lines in base
    and is deliberately not there until something needs it.
34. **`memory_nodes`, `memory_edges` and `embeddings` have accessors and tests but no
    writer.** They are P5's tables; creating them now is what keeps that plugin a reader of
    an existing file rather than a migration of a live one.

35. **Base owns the approval queue; G is notified, not asked.** RFC section 11 says "G owns the
    queue and timeouts". G runs in a node with no state file, no socket of its own and a
    read-only code path, so owning a queue would mean a second writer of the barebone's state
    file and a second RPC surface. What base does instead: it writes the rows, holds the blocked
    callers, expires them on a timer, and sends the guardian a one-way `notify{kind, data}` frame
    for `approval.pending`, `budget.exceeded`, `kill.switch` and `kill.resume`. `Link` answers
    that frame with `{ok: true}` and passes it on to whoever asked to be told — the only BEAM
    change this phase (plus its test).
36. **The approval id travels as `approval_id`.** `id` is the frame's own correlation id on the
    front door, the same reason `plugin_id` exists; a field literally named `id` overwrites it
    and the caller waits for a reply that is answered under a different number.
37. **`config.patch` of an env overlay stays ungated by default.** The RFC gates "base config
    changes"; RFC section 10's table makes L3 config agent-owned and auto. So `target: "base"`
    (new, patching `config.yml` itself) is always gated and the env overlay is gated only when
    `approval.gate_config_patch` is set.
38. **`snap.export` is the workspace push-out, and it exports the newest pack.** RFC section 8
    asks for "default clone-in / push-out with human review". Packs are self-contained
    (deviation 14), so the newest one *is* a bundle a human can `git` against; exporting a range
    would be a second format nothing reads yet.
39. **Token usage is read off the session log, not from a new `llm/usage` event.** Every
    `assistant/message` already carries the step's usage; counting it in `events.append` means
    the budget cannot disagree with the log and the harness needed no change to be metered.
40. **A budget breach halts the harness process; it does not unwind the turn.** Base has no way
    to reach into a running turn, and the harness is restartable by design (its sessions are in
    the log). So the hard stop is `SIGTERM` to that env's harness, no restart while halted, and
    every later `session.create`/`session.prompt` refused with the reason. `tenon run` prints
    the reason and exits non-zero because it watches for `budget.exceeded` and `kill.switch` in
    the stream it is already reading.
41. **Budget counters live in memory, not in the state file.** They are per boot: base restart
    or `tenon reset` (with `budget_reset_on_reset`) clears them. Persisting them would make a
    crash loop inherit a spend it cannot explain, and the log has the usage rows if a total is
    ever wanted.
42. **`tenon serve --http` is hand-rolled on tokio, not axum.** The RFC allows a feature-gated
    axum; four CGI-like routes, no JSON API, no middleware and no state do not pay for a web
    framework plus its tower/hyper-server tree in `--all-features` builds. The feature is still
    off by default, so nothing here is in the default binary.
43. **The `http` feature lives on `tenon-base` and is re-exported by `tenon-cli`.** `cargo build
    --release --features http -p tenon-cli` is the build that has the `serve` subcommand; the
    default binary does not list it at all.
44. **The pty test drives `script -q`, not a `forkpty` helper.** `attach --ui` needs a real
    terminal for `TIOCGWINSZ` and raw mode; `script` is in util-linux on every box this runs on
    and keeps the test free of `unsafe`.

45. **`runtime.register` authenticates with a second, per-env token, not the node token.**
    The node token is bound to an exact OS pid base spawned, which is what makes
    `node.register` unforgeable; a runtime base did not spawn has no such pid, so reusing
    that token would mean weakening the node check. Base generates a separate
    `runtime_token` per env instead, hands it to that env's harness in the environment and
    writes it to `run/rt-<env>.token` with mode 0600 for a runtime a human starts by hand.
    It is exactly as protected as `run/base.sock`, and it dies with the env.
46. **The default runtime is registered by base, not by the harness.** RFC section 2 has
    the runtime register with its parent; for the default runtime the parent is base and
    the runtime is base's own harness/worker/node A triple, so a registration frame from
    the harness would be base asking itself. Base registers it once the harness answers,
    with `loop.ping` as the health target — the same call the guardian's harness probe
    makes, so the contract's health claim and the guardian's probe cannot drift apart.
47. **`tenon rollback` is a local, base-must-be-down operation.** It restores
    `state.sqlite`, and that file has exactly one writer; copying over it while base holds
    it open would corrupt the thing being rescued. So `rollback` talks to no socket at
    all, refuses while `run/base.ready` exists, and names the pid that has to go first.
    `status --lkg` is local for the same reason: the LKG is what a human reads *because*
    base may not be reachable.
48. **The LKG manifest verifies the LKG copies and the installed plugins, not the
    workspace.** `config_hash`, `profile_hash` and `state_copy.sha256` are recomputed over
    `lkg/` (has the pinned copy itself been touched or truncated), and every pinned plugin
    must still be installed with the same hash (has an agent replaced a plugin version
    under a name the pinned profile resolves). Packs are the workspace history and are
    restored by `reset`, not by `rollback`.
49. **The privilege drop covers the host-side processes of an env, not the sandbox.**
    Node A and the harness are what `setuid` reaches; the worker already runs inside a
    container (its uid is the image's) or under Landlock (where it is the same process
    tree the drop applies to). RFC section 4 asks for "per-env OS privilege drop"; this is
    the half that is base's to give, and it is best effort — an unprivileged base logs the
    reason and keeps supervising rather than refusing to boot.
50. **`install-service` writes and enables, but never starts.** A unit that starts base the
    moment it is installed would boot a home the human has not chosen yet, possibly beside
    a base already running from a shell. The command prints the one line to run.
