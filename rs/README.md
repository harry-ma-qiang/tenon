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
| `storage` | `state.sqlite`: WAL, `synchronous=NORMAL`, `busy_timeout=5000`, `events`, `envs` |
| `sandbox` | the `Sandbox` trait plus `none`, `oci` (podman/docker) and `landlock` backends; `krun` is a P3.6 placeholder |
| `harness` | role stub (P3.3) |
| `worker` | role stub (P3.2); its public error type is already the wire SDK's |
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
| `profiles/guardian/` | an empty entry list; G mounts only barebone plugins |
| `erts/<version>-<sha>/` | the extracted BEAM release, read-only to base and to the nodes |
| `run/base.sock` | the front door |
| `run/base.lock` | an exclusive `flock`, held for the life of the base process; guards against a second `start` |
| `run/base.ready` | holds the base pid while it is up; how `tenon start` waits. Written atomically (temp file + rename) |
| `run/{base,guardian,root}.log` | stdout and stderr of base and of each node |
| `run/gateway-<env>.sock` | default `TENON_GATEWAY` unix socket for that env's node, passed to it and (oci/landlock) reachable from inside its sandbox |
| `envs/<env>/workspace/` | that env's sandbox workspace, bind-mounted (oci) or granted read-write (landlock) at the same path |
| `lkg/` | `config.yml`, `profiles/`, `state.sqlite` copied at every successful boot |

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
guardian:
  interval_ms: 2000
  failures: 6
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

**oci.** Default image `python:3.12-alpine` (has `python3`, `sh`, `timeout`); override
with the `TENON_SANDBOX_IMAGE` env var on base. Memory capped at `policy.ram_mb`
(default 512, via `--memory`), process count at `policy.pids_max` (default 256, via
`--pids-limit`). The workspace directory is bind-mounted at `/workspace` inside the
container. For gateway reachability: a `unix:` `TENON_GATEWAY` has its **directory**
(not just the socket file, which may not exist yet) bind-mounted read-write at the same
absolute path inside the container and the env var passed through unchanged, so a
plugin connecting to `TENON_GATEWAY` from inside sees the same path as the host; a
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

## Commands

| Command | What |
|---|---|
| `tenon start [--foreground] [--exit-on-detach] [--release-dir DIR] [--home DIR]` | boot G, boot the root env, open the front door. Without `--foreground` it re-execs itself detached (`setsid`), waits for `run/base.ready` and prints the pid |
| `tenon attach [--env NAME]` | print the status document, then stream the event log until Ctrl-C |
| `tenon stop [--all]` | stop every env, then G, then base; `--all` also reaps this home's dead-base sandbox leftovers afterward |
| `tenon reset [--env NAME]` | SIGTERM/SIGKILL that env, restore its LKG profile, start it again. G is untouched |
| `tenon status` | one JSON document: base, both nodes, and each node's fiber tree |
| `tenon sandbox reap [--all]` | remove stale sandbox containers for this home; works whether or not base is running. Without `--all`, only containers whose `tenon.base` pid is confirmed dead go; with it, every container for this home goes regardless of liveness. A human-facing counterpart to the boot-time reap, for a home nobody is about to `start` again soon |
| `tenon harness` / `tenon worker` | print `not implemented in P3.0`, exit 2 |

`--exit-on-detach` stops everything when the last **subscriber** disconnects. `status` and
`stop` connect without subscribing, so only `attach` holds the door open.

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
| `reset{env}` | guardian, CLI | kill, restore LKG, restart. Refused for `guardian` |
| `svc{env,name,method,args}` | CLI | forwarded to that env's node as a `svc` frame; the node proxies it to `:tenon.svc/4` against its own kernel root ctx and answers with the plugin's result or error |
| `sandbox.exec{env,cmd,args,timeout}` | CLI | run `cmd args..` inside that env's sandbox instance (`timeout` ms, default 30000); answers `{status,stdout,stderr,timed_out}`. P3.1 test/CLI aid — superseded by the worker's tool surface in P3.2 |
| `sandbox.destroy{env}` | CLI | destroy that env's sandbox instance now, without touching the node; the next `reset` (or node restart) creates a fresh one. Also a P3.1 test aid |
| `stop` | CLI | graceful shutdown of everything, base included |
| `status` | CLI | the snapshot plus one `tree` request per registered node |
| `subscribe{env}` | CLI | this connection starts receiving `{"t":"event",...}` frames; `env` keeps only that env's events plus the base-wide ones |

Requests to a node are answered outside the supervisor actor, so a `health` probe never
queues behind a `reset`. `reset` and `stop` do run inside it, for at most `stop_grace_ms`.
`sandbox.exec` and `sandbox.destroy` run the actual backend call in a `spawn_blocking`
task, not inside the actor, so a slow container exec never stalls `status`/`health`.

`status`'s per-node `sandbox` field is now an object rather than a bare id string:
`{"backend":"oci","id":"tenon-6af3f8eda318-root-171...","attach":"unix:/home/x/.tenon/run/gateway-root.sock"}`,
or `null` for the guardian.

## What base does when something dies

| Event | Base |
|---|---|
| `kill -9` base | nothing — the nodes notice their socket closing and stop themselves (~1.1 s). Any sandbox instance base owned keeps running too; nothing survives a SIGKILL to call `destroy`. There is nothing to do about this *at* the moment of the kill — the next `tenon start` (or `tenon sandbox reap`) of the same home reaps it, since the leaked container still carries this boot's `tenon.base` pid and that pid is now provably dead |
| SIGTERM/SIGINT base | graceful `stop`: envs first (each env's sandbox instance is `destroy`ed as part of stopping it), then G, then exit. The RPC reply for `stop` (and `AbortBoot`, its during-boot equivalent) is held until this full teardown — env kill, sandbox destroy, all of it — actually completes, not sent the moment shutdown starts; a caller that trusts "ok" and force-kills base shortly after (a test fixture's teardown, an impatient supervisor) would otherwise race an in-flight `podman stop`/`rm -f` and orphan the container anyway, the same failure mode as the row above, just self-inflicted |
| an env node dies | log `node.exit`, restore its LKG profile, restart it, up to `max_restarts`. The old sandbox instance is `destroy`ed before the new one is spawned |
| G dies | the same, plus a loud line on stderr; the env keeps running |
| the guardian sees N bad health answers | it sends `reset{env}`; base performs it (old sandbox instance destroyed, new one spawned, same as an env restart) |

## Tests

```
cd ../beam && MIX_ENV=prod mix release        # the integration tests need it
cd ../plugins/term && cargo build --release   # for the demo-plugin assertion
cd ../../rs
cargo build --release && cargo clippy --all-targets -- -D warnings && cargo fmt --check
TENON_RELEASE_DIR=$PWD/../beam/_build/prod/rel/tenon_beam cargo test
```

42 tests: `sandbox` unit 5, `boot.rs` 7, `storage` 3, `harness` 1, `worker` 1, `base` unit 2
(`token`, and `home::hash` — stable per home, distinct across homes), the 20-test
adversarial suite (19 as before P3.1's container hygiene pass, plus
`reap::a_leaked_container_with_a_dead_base_is_reaped_on_next_start`), `sandbox`'s 2-test
conformance suite and the 1-test gateway gate. `cli/tests/boot.rs` (7)
drives the real binary against a temp `TENON_HOME`: the role stubs, a missing base, boot
(both nodes registered, the guardian tree carrying `guardian` and `link`, the root tree
carrying the demo plugin, the LKG written, and — since `sandbox` now defaults to `auto`
and this box has podman/docker — the root env's `sandbox.backend` is `oci`), `reset` (new
pid for A, unchanged pid for G, the old process gone), `kill -9` base (both nodes gone
inside 5 s), `stop` (base and both nodes gone, socket and ready file removed) and an
unexpected `SIGKILL` of A (base brings it back with `restarts: 1`). Without a release they
print a skip line naming how to build one and pass.

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

Every fixture across `boot.rs`, `gateway_gate.rs` and the adversarial suite also sweeps its
own home with `tenon sandbox reap --all` on teardown (`Drop`), independent of whatever the
test itself asserted — several of these tests deliberately `kill -9` base to test its
resilience, which leaks that boot's container by design (see "What base does when
something dies" above) and this home will never `start` again to trigger the ordinary
reap, so the fixture reaps it directly instead of leaving it for `podman ps -a` to
accumulate across CI runs.

## Deviations from the RFC

1. **No `rollback`, `approve` or `run` subcommand, and no `wire` crate.** They belong to
   P3.5/P3.7 and to the P3.1 move of `sdk/rs`; `sdk/rs` stays where it is for now.
2. **`sdk/rs` is only a path dependency of `worker`**, which re-exports its `Error`/`Result`
   as the worker's public error type. Base does not speak fd 3/4 — it speaks the same frames
   over a socket — so nothing else needs the SDK until the worker becomes a wire plugin.
3. **The guardian's release directory is not `chmod`ed.** G and A are started from the same
   extracted release, so a permission change would apply to both and would fight the payload
   extractor. Read-only is enforced instead by `RELEASE_MODE=embedded` and
   `RELEASE_DISTRIBUTION=none` in the release (no code loading, no remote shell, no epmd) and
   by base never writing under `erts/`.
4. **`reset` answers as soon as the old node is dead and the new one is spawned**, not when
   it has registered. That keeps the answer inside the requesting node's deadline and keeps
   the actor free; `status` reports `registered: false` until the node is back.
5. **Only `events` and `envs` exist in `state.sqlite`.** `tool_results`, `snapshots`,
   `packs`, `blobs`, `memory_*`, `embeddings` and `episodes` arrive with the phases that
   write them (P3.2-P3.4); adding empty tables now would freeze schemas nothing has used.
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
