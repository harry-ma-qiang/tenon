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
| `sandbox` | the `Sandbox` trait plus the `none` backend; oci/landlock/krun are P3.1 |
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
sandbox: none
guardian:
  interval_ms: 2000
  failures: 6
```

The default profile mounts `plugins/term`'s release binary if it is built, otherwise
`playground/web/plugins/guard.py`, otherwise nothing. `TENON_DEMO_PLUGIN` names a binary
directly and `TENON_REPO` names the checkout; without either, base walks up from the working
directory looking for `kernel/src/tenon.erl`.

## Commands

| Command | What |
|---|---|
| `tenon start [--foreground] [--exit-on-detach] [--release-dir DIR] [--home DIR]` | boot G, boot the root env, open the front door. Without `--foreground` it re-execs itself detached (`setsid`), waits for `run/base.ready` and prints the pid |
| `tenon attach [--env NAME]` | print the status document, then stream the event log until Ctrl-C |
| `tenon stop` | stop every env, then G, then base |
| `tenon reset [--env NAME]` | SIGTERM/SIGKILL that env, restore its LKG profile, start it again. G is untouched |
| `tenon status` | one JSON document: base, both nodes, and each node's fiber tree |
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
| `stop` | CLI | graceful shutdown of everything, base included |
| `status` | CLI | the snapshot plus one `tree` request per registered node |
| `subscribe{env}` | CLI | this connection starts receiving `{"t":"event",...}` frames; `env` keeps only that env's events plus the base-wide ones |

Requests to a node are answered outside the supervisor actor, so a `health` probe never
queues behind a `reset`. `reset` and `stop` do run inside it, for at most `stop_grace_ms`.

## What base does when something dies

| Event | Base |
|---|---|
| `kill -9` base | nothing — the nodes notice their socket closing and stop themselves (~1.1 s) |
| SIGTERM/SIGINT base | graceful `stop`: envs first, then G, then exit |
| an env node dies | log `node.exit`, restore its LKG profile, restart it, up to `max_restarts` |
| G dies | the same, plus a loud line on stderr; the env keeps running |
| the guardian sees N bad health answers | it sends `reset{env}`; base performs it |

## Tests

```
cd ../beam && MIX_ENV=prod mix release        # the integration tests need it
cd ../plugins/term && cargo build --release   # for the demo-plugin assertion
cd ../../rs
cargo build --release && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cargo test
```

14 tests plus a 19-test adversarial suite. `cli/tests/boot.rs` (7) drives the real binary
against a temp `TENON_HOME`: the role stubs, a missing base, boot (both nodes registered, the
guardian tree carrying `guardian` and `link`, the root tree carrying the demo plugin, the LKG
written), `reset` (new pid for A, unchanged pid for G, the old process gone), `kill -9` base
(both nodes gone inside 5 s), `stop` (base and both nodes gone, socket and ready file removed)
and an unexpected `SIGKILL` of A (base brings it back with `restarts: 1`). Without a release
they print a skip line naming how to build one and pass. The unit tests are `storage` (3:
append order, env upsert, the day-one pragmas), `sandbox` (2), `harness`, `worker` and
`base::token` (1 each).

`cli/tests/adversarial/` (19, same skip-without-a-release rule) is the P3.0 hardening suite:
double start refusal and survival of the first base, a crashed base's stale `run/` files
recovered by the next start, a five-round reset storm with no orphaned pids, `stop` racing a
`reset`, SIGTERM during boot leaving no zombie or orphaned BEAM process, twenty parallel
`status` calls during a `reset`, a corrupt `profile` and a corrupt `state.sqlite` each
restored from LKG on `reset`, guardian/env crash and restart-limit scenarios, a frozen agent
reset by the guardian without SIGCONT confusing base afterwards, two `attach` subscribers and
`--exit-on-detach`, and RPC abuse (garbage bytes, an oversized frame header, an unknown
method, a half-open connection, a forged `node.register` from the CLI socket).

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
