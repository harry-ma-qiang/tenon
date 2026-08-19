# tenon_beam — the node release

The BEAM half of the P3.0 barebone. One Elixir release, `tenon_beam`, that base starts
twice: once as the **guardian node G** and once per **environment** as an **agent node A**.
Both run the same code and differ only by environment variables.

```
base (rust)                      node (this release)
  run/base.sock  <--- UDS ------  Tenon.Beam.Link      one connection, outbound
                                    |
                                  kernel (../kernel)
                                    +-- loader   (../loader, mounts the profile)
                                    +-- link     (this)
                                    +-- guardian (this, role = guardian only)
                                    +-- gateway  (this, role = agent only) <--socket-- plugin
```

These plugins are mounted directly by `Tenon.Beam.Boot`, outside the loader, because they
belong to the barebone and must not be reachable by an agent editing the profile.

## Boot

`bin/tenon_beam start`, with:

| Variable | Meaning |
|---|---|
| `TENON_ROLE` | `guardian` or `agent`. Unset means "not a Tenon node": the application starts empty (what `mix test` and `iex -S mix` want) |
| `TENON_ENV` | the environment name this node is, e.g. `root` |
| `TENON_BASE_SOCK` | path of the base front door; `Link` connects to it |
| `TENON_PROFILE` | path of the profile entry list; `registry.yml` next to it supplies the `name => spec` rows |
| `TENON_GUARDIAN_TARGET` | guardian only, the env to watch (default `root`) |
| `TENON_GUARDIAN_INTERVAL_MS` | guardian only, probe interval (default 2000) |
| `TENON_GUARDIAN_FAILURES` | guardian only, consecutive failing passes before `reset` (default 6) |
| `TENON_GUARDIAN_PROBE_TIMEOUT_MS` | guardian only, deadline of one probe call and the line above which it counts as wedged (default 5000) |
| `TENON_GUARDIAN_PROBES` | guardian only, `:`-separated paths of the extra probe executables base approved |
| `TENON_GATEWAY` | agent only, listen address: `unix:<path>` or `tcp:<host>:<port>` (default `unix:<TENON_HOME or ~/.tenon>/run/gateway-<TENON_ENV>.sock`) |

`rel/env.sh.eex` sets `RELEASE_DISTRIBUTION=none` (no distribution, no epmd, no cookie) and
`RELEASE_MODE=embedded` (all modules preloaded, no code loading from disk at runtime), so a
node cannot be reached from another BEAM and does not read new beams out of its release
directory. Base treats the release directory as read-only and never writes into it; every
node writes only under `~/.tenon/`.

`mix release` includes ERTS, so the tree under `_build/prod/rel/tenon_beam` is
self-contained and is what base ships as its payload.

## Frames

The same shape as the wire on fd 3/4: a **4-byte big-endian length** followed by that many
bytes of JSON, with the method in `t`. On the socket the framing is `{:packet, 4}`.

A request carries an `id`; the answer is `{"t": "rep", "id": <id>, "result": ...}` or
`{"t": "rep", "id": <id>, "error": "..."}`. Ids are per direction, so base and node number
their own requests independently and never collide.

Node to base:

| Frame | Fields | Meaning |
|---|---|---|
| `node.register` | `role`, `env`, `pid` | first frame, no `id`, no answer. `pid` is the OS pid base signals |
| `health` | `id`, `env` | guardian only: how is that env doing |
| `reset` | `id`, `env` | guardian only: restart that env from LKG |

Base to node:

| Frame | Fields | Answer |
|---|---|---|
| `health` | `id` | `{ok, role, env, pid, fibers, failed}` |
| `tree` | `id` | `{tree: <kernel tree, pids as text>}` |
| `reload` | `id` | `{ok: true}` after `Tenon.Loader.reload/1` |

Anything else is answered `error: "unknown_method:<t>"`.

## Link

`Tenon.Beam.Link` connects **outbound** in `load/2`; a connect failure fails the fiber, so
a node with no base never pretends to be up. It publishes the kernel service `link`, whose
only method is `request(method, params)` returning `{:ok, result}` or `{:error, reason}`
(`{:error, :timeout}` after 15 s). That service is how `Guardian` talks to base.

When the socket closes — `tcp_closed`, `tcp_error`, or base being `kill -9`ed — the node
**stops**: `System.stop(0)` for the graceful path and a hard `System.halt(0)` 2 s later as
the backstop. This is the only mechanism that takes nodes down with base, and it needs no
supervision from the OS. Measured on this box: 1.1 s from socket close to node exit.

`halt: false` and `notify: <pid>` in the fiber config replace the halt with a message; the
tests use them, base never sets them.

## Guardian

`Tenon.Beam.Guardian` injects `link`, so it stays `pending` until `Link` is active. Every
`interval` it runs one pass of `Tenon.Beam.Guardian.Probes` against `target`; a pass with
at least one failing probe is a strike, a clean pass clears the count, and at `failures`
strikes it sends `reset{env: target, probes: [names]}` to base and starts over. It is
mounted only in the guardian node, and base is what actually performs the reset: the
guardian never touches an OS process.

The core set is fixed, in this order: `base` (base's own `status`, whose row for the target
env the next probes read), `env` (`health`), `tree` (the kernel tree's root fiber is
`active`), `worker` (`svc worker.ping`), `harness` (`svc loop.ping`), `budgets` (that env's
`budget.halted`) and `violations` (new `violation` / `budget.exceeded` rows in its log,
counted once by carrying the newest event id). Any probe call that reaches
`probe_timeout` also fails as `wedged`; `probe_timeout` is passed to `Link.request/4` as
the per-call deadline, so a wedged base cannot hold a probe pass for the 15 s the ordinary
`link` timeout allows. The worker and harness probes ask for a `pong` only when base's
status says that piece is `ready` — an env that is still booting owes no answer.

Extra probes are executables base approved (`TENON_GUARDIAN_PROBES`), run with the env name
as their only argument under the same timeout; a non-zero exit is a failing probe named
after the file. Which files those are is base's decision (sha256 in base's config), never
the guardian's.

## Gateway

`Tenon.Beam.Gateway` is the in-sandbox registration path from RFC section 6: it listens on
`TENON_GATEWAY` (`{:packet, 4}`, binary) and, for every accepted connection, calls
`:tenon.mount(ctx, %{socket: sock, id: "gw-<n>"})` under its own ctx — a kernel
socket-backed external fiber (`../kernel`, wire v1.2). One acceptor process loops on
`:gen_tcp.accept/1` and hands each socket to a short-lived process that claims it
(`:gen_tcp.controlling_process/2`, so the accept loop is never blocked on a slow or silent
client) and mounts it; the `Gateway.Server` GenServer monitors each resulting fiber and
logs accept/disconnect. Mounting under the gateway's own ctx means every connection fiber
is a child of the gateway fiber, so unmounting the gateway (or its node dying) drops all of
them for free, the same cascade that already unwinds any other parent/child mount.
Mounted only in agent-role nodes — a guardian node has no sandbox to register plugins from.

## Check

`Tenon.Beam.Check` is the kernel contract suite, shipped as a release artifact so an
installed machine can verify a candidate `tenon.beam` without a development tree.
`bin/tenon_beam eval 'Tenon.Beam.Check.main()'` with `TENON_CHECK_BEAM` set loads that beam
into this fresh node, runs every point and prints one JSON report; base drives it as
`tenon check kernel [--beam PATH]`. The points, the wire plugin (a process of the same VM on
a loopback socket, so no python is needed) and the `TENON_KERNEL_CONTRACT=1` versioning rule
are documented in `../kernel/README.md`.

## Tests

```
mix compile --warnings-as-errors && mix format --check-formatted
mix credo --strict && mix test && MIX_ENV=prod mix release
```

38 tests. `test/link_test.exs` (14) covers register, `health`, `tree`, `reload`, the unknown
method, request correlation in both outcomes, the node-stop on close, and the failed load
without a socket. `test/guardian_test.exs` (15) covers one test per probe kind — base not answering, an
unhealthy env, a root fiber that is not active, a worker and a harness that do not answer
`ping`, a booting worker that is deliberately not a failure, a worker base reports as
failed, a halted budget, a violation row counted once, a call that outlives
`probe_timeout` failing as `wedged`, and an extra probe that exits non-zero while its
approved sibling passes — plus the quiet path, the reset after N failing passes carrying
the probe names, recovery clearing the count, and the target name. Both run against
`Tenon.Beam.Test.Base`, a fake base on a real unix socket that answers per method (and per
service name for `svc`, so the worker and harness probes can be failed separately).
`test/registry_test.exs` (2) covers the
builtin names and the spec shapes. `test/check_test.exs` (3) runs the contract suite against the compiled kernel (every point
passes), against a corrupted beam (one `load` failure naming the reason, no point run) and
against a contract version the suite does not implement (refused by name).
`test/gateway_test.exs` (4) starts a kernel and a gateway on a temp UDS path and connects
fake clients directly (no base needed): a `:tenon.svc` call reaches a connected client,
disconnecting fails that client's fiber and drops its service, a second client gets its own
fiber, and unmounting the gateway drops an active connection's service.
