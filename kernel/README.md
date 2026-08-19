# tenon — atom kernel

One Erlang module (`src/tenon.erl`, ~990 lines, zero dependencies, OTP 27) that is the
whole Tenon microkernel: a plugin/service/hook registry with lifecycle, dependency
gating, event dispatch, effect disposal, and a wire protocol for out-of-VM plugins.

Everything else — config loader, YAML tree, schema validation, broker bridges, SDKs, the
`tenon` CLI — is a plugin or a caller outside the kernel. The kernel has no comments by
design; this file is the explanation.

Design record and phase history: `../NOTES.md` (section 9 is the spec this implements).

## Purpose

Cordis-style composition on the BEAM. A running system is a tree of *fibers*. Each fiber
owns one plugin instance. Plugins publish *services* and *hooks*, consume services by
declaring `inject/0`, and register *effects* whose disposers unwind in reverse order.
The kernel guarantees that when a fiber dies — cleanly or violently — nothing it
registered survives it.

## Architecture and process model

Two kinds of `gen_server`, both implemented by the same module, distinguished by their
state record (`#k{}` for the kernel, `#f{}` for a fiber).

```
                 ETS (owned by the kernel, unnamed, public)
      fibers          services         hooks           deps       seq
   {pid,uid,id,     {name,impl,     {{event,seq},    {name,      counters
    parent,module,   owner}          ref,owner,fun}   fiber}      uid/append/
    status,inject,                                    (bag)       prepend/req
    epoch,error}
        ^                ^                ^              ^
        | single writer  | writer=owner   | writer=owner | writer=owner
        |                |                |              |
   +----+----------------+----------------+--------------+--------+
   |                                                              |
 kernel gen_server  --start_link/monitor-->  fiber gen_server ... fiber
   |  mount/unmount, DOWN sweep, refresh casts   |  load/unload, disposer stack,
   |  never calls a fiber, never runs user code  |  own status row, own port
   |                                             |
   +-- root fiber (module = undefined) mounted synchronously in init/1
```

* **Kernel.** Starts fibers (`gen_server:start_link` from the kernel process, so it is
  linked to every fiber and traps exits), sweeps hook/service/fiber rows by owner on
  `EXIT`, cascades `unmount` to the dead fiber's children, and casts `refresh` to fibers
  whose `inject` list mentions a service that just appeared or vanished — found by
  looking the changed names up in the `deps` index, never by scanning `fibers`. It is
  not on any hot path: dispatch never touches it.
* **`deps`.** A bag of `{ServiceName, FiberPid}`, one row per injected name of every live
  fiber. Written by the fiber itself when its `inject` list becomes known (`init/1` for an
  in-VM plugin, the `hello` frame for an external one, rewritten on every re-`hello`), and
  removed by the kernel's `EXIT` sweep and by the fiber's `terminate/2`. It is an index
  only: `inject` stays in the `fibers` row, so `tree/1` is unaffected.
* **Fiber.** Runs the plugin's `load/2` in its own process, accumulates disposers,
  writes its own status row, and (for external plugins) owns the `Port` and a pending
  request map. It never blocks on the wire: every wire request is a `gen_server` reply
  parked behind a request id with a deadline timer.
* **Dispatch.** `emit`/`call`/`bail` read the `hooks` ordered_set straight from ETS in
  the *caller's* process and run the hook funs there. Hook order is table order:
  `append` uses an increasing sequence, `prepend` a decreasing negative one, so the most
  recent prepend runs first.
* **Wire-originated work.** `emit`, `call` and `svc` frames coming *from* a plugin are
  executed in a freshly spawned worker process, never in the port owner, so a plugin can
  call a service it provides itself without deadlocking its own fiber.

**Lifetime.** A kernel is a `gen_server`, so one started with `start_link/0,1` dies with
the process that started it. Supervise it: put `start_link` under a supervisor, where the
supervisor is the long-lived parent. For scripts, tests and the shell, use `start/0,1`,
which is unlinked and outlives its caller. Either way fibers stop when the kernel stops —
they are linked to it and close their ports on the way out.

## Invariants

1. The kernel never calls a fiber synchronously and never runs user code. Kernel to
   fiber is `cast` only; fiber to kernel may be `call`.
2. The kernel emits no events. Fibers emit `internal/plugin`, `internal/status` and
   `internal/service` about themselves.
3. Dispatch runs in the caller process, in registration order. `emit` isolates each hook
   (`try` + `logger:error`); `call` and `bail` let errors propagate.
4. Every registration returns a disposer. `effect/2` runs the body immediately in the
   caller and hands the resulting disposer to the owning fiber: `gen_server:call` from a
   foreign process, `send(self(), ...)` from inside the fiber (mailbox FIFO keeps order,
   no deadlock, no process dictionary). Disposers run in reverse order in the fiber.
5. Hook and service rows carry the owner fiber pid. The kernel monitors every fiber; on
   `EXIT` it deletes rows by owner and re-evaluates dependents. `exit(Fiber, kill)`
   leaves no rows.
6. `load/2` raising, returning `{error, R}`, or returning anything unexpected puts the
   fiber in `failed`. The process stays alive with the error in its row, so it shows in
   `tree/1` and `restart/1` retries. Any other crash is handled by the `EXIT` sweep.
7. Inject epoch is `[{Name, ProviderPid}]`. pending + all present -> load; active + one
   missing -> unload -> pending; active + provider pid changed -> unload + load.
8. A child fiber is mounted as an effect of its parent, at the position where it was
   mounted. Parent unload therefore disposes children in reverse effect order, not
   unconditionally first.
9. Many kernels per VM. All tables are unnamed and reached through the ctx.
10. **The ctx you pass names the owner.** Any process may register on behalf of a fiber:
    `mount/2`, `on/3,4`, `provide/3` and `effect/2` all use `maps:get(fiber, Ctx)` as the
    owner, not `self()`. Hand a plugin `Ctx#{fiber := OtherFiber}` and its registrations
    belong to `OtherFiber` — they sit at that point of *its* disposer stack, unwind in
    reverse when it unloads, and vanish when it dies. Do it from the fiber itself or from
    a process that is not blocked on that fiber: a foreign registration is a
    `gen_server:call` into the owner, so a process the owner is waiting on will deadlock.

## API

Ctx is a plain map: `#{kernel := pid(), tabs := map(), fiber := pid()}`. `fiber` is the
*owner* every registration below is charged to, and it is a plain field: passing
`Ctx#{fiber := OtherFiber}` registers on that fiber's behalf (invariant 10). A group
loader uses this to mount children into a group fiber so one `unmount` takes the group
and everything under it.

| Function | Meaning |
|---|---|
| `start_link()` / `start_link(Opts)` | start a kernel linked to the caller (supervised use). `Opts`: `deadline` (default 30000 ms), `grace` (default 5000 ms), `max_frame` (default `TENON_MAX_FRAME` or 1048576 bytes) |
| `start()` / `start(Opts)` | same, unlinked: survives a short-lived caller (scripts, tests, shell) |
| `stop(Kernel)` | stop the kernel; every fiber stops with it |
| `root(Kernel) -> Ctx` | ctx of the root fiber |
| `tree(Kernel) -> map()` | nested `#{pid, uid, id, parent, module, status, inject, epoch, error, children}` |
| `status(Fiber) -> pending \| loading \| active \| failed \| unloading \| disposed` | settle point: recomputes the epoch, blocks while the fiber is mid-handshake |
| `mount(Ctx, Spec) -> {ok, Fiber}` | `Spec` = `#{module := M, config => C, id => Id}` or `#{cmd := Cmd, args => [..], env => [..], config => C, id => Id}`. Child of `Ctx`'s fiber, settled before return. If that fiber dies mid-mount the new child is unmounted again and the call raises `{owner_gone, Fiber, Reason}` |
| `unmount(Fiber) -> ok` | run disposers in reverse, sweep rows, stop. Idempotent |
| `restart(Fiber)` / `restart(Fiber, Config)` | unload then load again; the two-arity form replaces the config (this is the old `update`) |
| `effect(Ctx, Fun) -> Disposer` | `Fun` runs now; returning a 0-arity fun registers it, `ok` / `undefined` / `nil` registers nothing |
| `on(Ctx, Event, Fun)` / `on(Ctx, Event, Fun, #{prepend => true})` | register a hook, returns a disposer |
| `emit(Ctx, Event, Args) -> ok` | broadcast; hooks isolated, errors logged |
| `call(Ctx, Event, Args, Terminal) -> Result` | waterfall. Hook is `fun(A.., Next)`; `Next(A'..)` rewrites args downstream; not calling `Next` short-circuits; `Terminal` is the default. Max 5 args |
| `bail(Ctx, Event, Args) -> Result \| undefined` | first hook returning something other than `undefined` wins |
| `provide(Ctx, Name, Impl) -> Disposer` | duplicate name raises `{service_exists, Name}` |
| `get(Ctx, Name) -> Impl \| undefined` | raw impl; for a wire service this is the opaque `{tenon_wire, Fiber, Name}` |
| `svc(Ctx, Name, Method, Args) -> Result` | module impl -> `Impl:Method(Args..)`; `fun/2` -> `Impl(Method, Args)`; wire ref -> request to the owning port |

Plugin module callbacks: `inject() -> [atom()]` (optional) and
`load(Ctx, Config) -> ok | {ok, Disposer} | {error, Reason}`.

```erlang
%% an in-VM plugin
-module(my_plugin).
-export([inject/0, load/2]).
inject() -> [db].
load(Ctx, Config) ->
    tenon:on(Ctx, 'request/before', fun(Req, Next) -> Next(Req#{seen => true}) end),
    tenon:provide(Ctx, cache, ?MODULE),
    tenon:effect(Ctx, fun() -> Fd = open(Config), fun() -> close(Fd) end end),
    ok.
```

## Wire (external plugins)

Transport: Erlang `Port` opened with `nouse_stdio`, `{packet, 4}`, `binary`, payload JSON
(OTP 27 `json`). The frame set is transport-independent; a socket transport or an ETF
codec is one extra clause.

**Wire v1.1 — fd 3 and fd 4.** The plugin reads frames from **file descriptor 3** and
writes frames to **file descriptor 4**. Its own stdin, stdout and stderr are untouched
and inherited from the VM, so `print`, `console.log` and stack traces are free for logs
and never corrupt the protocol. A frame is a 4-byte big-endian length followed by that
many bytes of JSON — unchanged from v1.0, only the descriptors moved.

```python
import os, json, struct
wire_in, wire_out = os.fdopen(3, "rb", 0), os.fdopen(4, "wb", 0)

def send(frame):
    body = json.dumps(frame).encode()
    wire_out.write(struct.pack(">I", len(body)) + body)

def recv():
    head = readn(4)                      # loop until 4 bytes or EOF
    return json.loads(readn(struct.unpack(">I", head)[0]))

send({"t": "hello", "inject": []})
print("logs go to stdout, the wire does not", flush=True)
```

A shell plugin can simply move the descriptors: `python3 -c '...' <&3 >&4` (see
`../playground/plugins/shell_echo.sh`).

**Wire v1.2 — socket transport.** `mount(Ctx, #{socket => Sock})` treats an
already-connected `gen_tcp`/local-domain socket exactly like a port-backed external
plugin: same hello/load/on/provide/emit/call/svc/next/rep frames, same deadlines, same
frame cap, same `tree`/`status` rows. `Sock` must be `{packet, 4}, binary}`; `mount/2`
transfers its controlling process to the new fiber and only then sets `{active, true}`,
so no frame can be delivered to the wrong process during the handoff (the documented
`gen_tcp:controlling_process/2` race). The kernel never listens itself — some other
process (typically a gateway plugin sitting in front of a listen socket) accepts the
connection and calls `mount/2`; the kernel only ever sees the accepted socket. The socket
closes (`tcp_closed` / `tcp_error`) the same way a spawned process exiting does — the
fiber goes `failed` — and `unmount` sends `unload` then closes the socket after `grace`,
mirroring the port backstop. There is nothing to respawn a closed socket with, so
`restart/1,2` on a socket fiber fails the fiber instead of trying.

Kernel to plugin:

| Frame | Fields | Meaning |
|---|---|---|
| `load` | `req`, `config` | become active; answer with `rep` |
| `unload` | — | release everything and exit (see below) |
| `hook` | `req`, `hook`, `event`, `args`, `mode` | a hook you registered fired. `mode: "emit"` has no `req` answer; `mode: "call"` expects `next` or `rep` |
| `result` | `req`, `result` | downstream result of a `next` you sent with `await: true` |
| `svc` | `req`, `name`, `method`, `args` | someone called your service; answer with `rep` |
| `rep` | `id`, `result` \| `error` | answer to your `call` / `svc` request |

Plugin to kernel:

| Frame | Fields | Meaning |
|---|---|---|
| `hello` | `inject` | first frame, always. Declares the injected service names |
| `on` | `hook`, `event`, `arity`, `mode`, `prepend` | register a hook (`hook` is your own id) |
| `off` | `hook` | remove it |
| `provide` / `unprovide` | `name` | publish / withdraw a service |
| `emit` | `event`, `args` | fire and forget |
| `call` | `id`, `event`, `args` | waterfall; answered with `rep{id}` |
| `svc` | `id`, `name`, `method`, `args` | call another plugin's service; answered with `rep{id}` |
| `next` | `req`, `args`, `await` | continue a `call`-mode hook. `args` must keep the arity. `await: true` asks for the downstream result before you finish |
| `rep` | `req`, `result` \| `error` | answer a `load`, `hook` or `svc` request |

Example session (kernel `>`, plugin `<`):

```
<  {"t":"hello","inject":["db"]}
                                     ... fiber stays pending until db exists
>  {"t":"load","req":1,"config":{"path":"/tmp/x"}}
<  {"t":"on","hook":2,"event":"wire/call","arity":1,"mode":"call"}
<  {"t":"provide","name":"pysvc"}
<  {"t":"rep","req":1,"result":"ok"}
                                     ... fiber active
>  {"t":"hook","req":7,"hook":2,"event":"wire/call","args":[1],"mode":"call"}
<  {"t":"next","req":7,"args":[2],"await":true}
>  {"t":"result","req":7,"result":20}
<  {"t":"rep","req":7,"result":21}
>  {"t":"svc","req":8,"name":"pysvc","method":"add","args":[1,2]}
<  {"t":"rep","req":8,"result":3}
>  {"t":"unload"}
                                     ... plugin exits, port closes
```

Rules:

* **Known error names are normalized at the kernel boundary.** A `rep` frame carrying
  `error` is answered as `{error, Reason}`. If the string is one of the kernel's own error
  names — `"frame_too_large"`, `"timeout"`, `"plugin_gone"` — it becomes the *atom* of the
  same name, so a plugin SDK that refuses to send an oversized frame itself and a kernel
  that refuses to write one produce the identical `{error, frame_too_large}`. Any other
  error string is passed through as a binary (`{error, <<"nope">>}`), and a failed `load`
  reply becomes `{plugin_error, Reason}` with the same normalization.
* Every kernel-to-plugin request carries a deadline (`deadline` option, default 30 s).
  On expiry a pending `svc` / `hook` call answers `{error, timeout}`; an expired `load`
  or `hello` fails the fiber and closes the port.
* Plugin exit while loaded -> fiber `failed` with `{exit_status, N}`, all its rows swept.
* `unmount` (and any lifecycle unload) sends `unload`, then waits up to `grace` (default
  5 s) for the process to exit, then closes the port and `SIGKILL`s the OS process if it
  is still there. A plugin's process lifetime therefore equals its loaded state:
  `restart` and a regained dependency re-spawn the command.
* Control plane only. **Bulk data goes by handle, never over the wire.** A plugin holding
  PTY bytes, a DOM tree, a file or a token stream returns a *handle* — a path, a unix
  socket, a URL, an fd, a stream endpoint — and the two plugins talk over that channel
  themselves. The kernel only brokers discovery through services. The frame cap below is
  what enforces the rule.
* Every plugin process is spawned with two environment variables appended to the spec's
  `env`: `TENON_MAX_FRAME` (the cap in bytes) and `TENON_KERNEL_DEADLINE` (the request
  deadline in ms). An SDK reads them at startup and sizes its own buffers and timeouts
  accordingly.

### Frame size cap

Every frame in both directions is capped. The limit is the `max_frame` kernel option in
bytes; if it is absent, the `TENON_MAX_FRAME` environment variable of the *VM* is used
when it parses as a positive integer; otherwise the default is 1048576 (1 MB).

```erlang
{ok, K} = tenon:start(#{max_frame => 4194304}).   %% 4 MB
```

* **Outgoing** (kernel to plugin): the frame is encoded, and if it exceeds the cap it is
  *not* written to the port. The event is logged, and a pending request — a `svc` call, a
  `call`-mode hook, a `result` continuation — is answered `{error, frame_too_large}`
  instead of being parked. An oversized `load` frame (a huge config) fails the fiber.
* **Incoming** (plugin to kernel): the frame is logged and dropped, and if it correlates
  to a pending request that request is answered `{error, frame_too_large}`. Note the
  asymmetry: `{packet, 4}` gives the emulator no way to refuse a large packet before
  reading it, so by the time the cap is checked the bytes are already in the VM. The cap
  therefore protects the *system* from acting on oversized payloads, not the VM from
  receiving them. **Plugins must respect `TENON_MAX_FRAME` themselves**; a plugin that
  ships a 100 MB frame still costs one 100 MB allocation before it is thrown away.

## Constraints

- `stop/1` with live external plugins closes their ports abruptly; the OS may print a benign
  EPIPE notice from the child. Unmount children first for a quiet shutdown (graceful kernel
  stop is deferred).

* **A hook must not synchronously call its own fiber.** Dispatch runs in the caller, so a
  hook that does `tenon:status(self_fiber)` or `tenon:svc/4` into its own fiber while
  that fiber is emitting will deadlock (with `call`/`bail`) or be caught and logged
  (with `emit`). Do the work asynchronously instead. Wire plugins are exempt: their
  inbound `emit`/`call`/`svc` frames run in a spawned worker.
* Same rule between parent and child during `load/2`: a child that calls its parent
  synchronously while the parent is inside `mount/2` will deadlock.
* Max hook arity for `call` is 5; more raises `{too_many_args, N}`.
* Event and service names arriving over the wire are converted with `binary_to_atom`.
  The wire is a trusted control plane, not an untrusted input surface.
* `emit`, `call` and `bail` share one hook table. A hook registered for `call` (arity
  N+1) will fail with a bad arity if the same event is `emit`ed, and vice versa. Pick one
  mode per event name.
* Config and results crossing the wire are JSON: atoms become strings, tuples become
  arrays, anything unencodable becomes its `~p` text. Keep wire configs to maps,
  binaries, numbers and booleans.

## Hot swap

The kernel and every fiber run the same module, so one load swaps them all:

```
1> c(tenon).          %% or l(tenon) after erlc
{ok,tenon}
```

`gen_server` calls `code_change/3` on both records (`#k{}` and `#f{}`) at the next
callback. All shared state lives in ETS owned by the kernel process, which is not
restarted, so hooks, services and status rows survive the swap untouched. When a record
gains a field, add a clause to `code_change/3` that rebuilds the old tuple; that is the
only place that needs to know about versions.

In-VM plugin code:

```
2> l(my_plugin).
3> tenon:restart(Fiber).     %% unload -> disposers -> load with the new code
```

External plugin: edit the script, then `tenon:restart(Fiber)` — the fiber sends `unload`,
waits for the process to exit, and re-spawns `cmd` with the same spec. Use
`tenon:restart(Fiber, NewConfig)` to change the config at the same time.

## Performance

From the smoke tests in `test/tenon_test.exs` (OTP 27.3, arm64, one core busy):

| Workload | Result |
|---|---|
| 100 000 `emit` with 3 hooks | ~107 ms, ~930 000 emits/s |
| 10 000 wire round trips (`svc` to a python3 plugin) | ~353 ms, ~28 000 round trips/s |
| 100 000 `emit` with a 10 000-row hooks table | ~1.3x a 3-row table (131 ms vs 100 ms) |
| 100 provide/unprovide cycles with 10 000 fibers | ~0.18 s (~0.52 s before the `deps` index) |

Dispatch cost is one `ets:select` on an ordered_set plus one `apply` per hook, in the
caller process; nothing is serialised through the kernel. The wire number is dominated by
the JSON encode/decode and the pipe round trip in python.

Dispatch scales with the number of *matching* hooks, not the table size: a partial-key
select on an ordered_set seeks to the `{Event, _}` range, so a 10 000-row hooks table
costs about 1.3x an empty one.

`provide` / `unprovide` notification used to be the one O(total fibers) path: `notify/2`
did an `ets:tab2list` of the fibers table per changed name. The `deps` bag replaced that
with one `ets:lookup` per name, so the cost is now O(dependents of that name). The
adversarial scale test (`test/tenon_adversarial_test.exs`, 100 provide/unprovide cycles of
one service with one real dependent among 10 001 fibers) went from **~520 ms to ~180 ms**
— a 2.8x improvement on the whole mount/unmount cycle; what is left is the fiber spawn,
load and settle, not the notification.

## Tests

```
cd kernel
mix compile              # erlc with warnings_as_errors
mix format --check-formatted
mix test
mix test --seed 1        # order-independent; seeds 1..5 verified
```

66 tests in three files. `test/tenon_test.exs` (43) is the whole P1 acceptance list
(load/unload, reverse disposers, kill sweep, inject wait/lose/regain/swap, prepend,
waterfall rewrite and short-circuit, parent cascade, cross-fiber mount into a group ctx
unwound by unmount and by kill, failed -> restart, internal events,
two kernels, 500-fiber stress), a python3 plugin written to a temp file for the wire cases
(hello with inject, emit-mode hook, call-mode hook with `next` + `await` and
post-processing, provide + svc, off + unprovide, exit 3 on load -> failed, unmount ends the
OS process, request timeout, noisy stdout/stderr, oversized reply, cap by option and by
`TENON_MAX_FRAME`, plugin environment, error-name normalization), and the two perf smoke
tests above.
`test/tenon_adversarial_test.exs` (18) covers hot swap, scale, concurrency and wire abuse.
`test/tenon_socket_test.exs` (5) mounts a UDS-connected python3 plugin the test itself
listens for and accepts (standing in for a gateway): hook + svc round trip, `unmount`
closing the socket and ending the process, `SIGKILL` of the plugin failing the fiber,
`restart` failing a socket fiber outright, and an oversized reply over the socket.

## The contract and `tenon check kernel` (P3.7)

The kernel is the one L1 artifact of RFC section 10: an agent may replace `tenon.erl` in its
own environment, but only if the replacement still keeps this file's promises. What
"promises" means is a suite, not a prose list, and the suite has to run **on the machine
that installs the kernel**, where there is no development tree, no `mix` and no test files.

So the contract suite ships inside the beam release as ordinary code
(`../beam/lib/tenon/beam/check.ex` and `check/`), and base runs it:

```
tenon check kernel                      # the tenon.beam the release ships
tenon check kernel --beam /tmp/new.beam # a candidate an agent built
```

Base runs `bin/tenon_beam eval 'Tenon.Beam.Check.main()'` in a fresh node with
`TENON_CHECK_BEAM` naming the candidate. The suite purges `tenon`, loads that file with
`code:load_binary/3` (a release runs in embedded mode, so a candidate beam is on no code
path and cannot be loaded any other way), runs every point against it and prints one JSON
document; the exit status is 0 only if every point passed.

| Point | What it asserts |
|---|---|
| `exports` | the module exports every function of the API table above, at every arity |
| `mount_unmount` | a plugin loads on `mount`, shows in `tree`, and `unmount` runs its disposers and ends the fiber |
| `disposers` | disposers run in reverse registration order |
| `kill_sweep` | `exit(Fiber, kill)` leaves no fiber, service or hook row behind |
| `inject` | a dependent waits for its provider, loads when it appears, unloads when it goes |
| `hooks` | `emit` order and isolation, `prepend`, the `call` waterfall rewriting and short-circuiting, `bail` |
| `provide_svc` | module impl and `fun/2` impl through `svc`, `get` on an absent name, duplicate `provide` raises |
| `socket_fiber` | wire v1.2: `hello`, `load`, `provide`, a `svc` round trip, a `call`-mode hook answered with `next`, and `unmount` closing the socket |
| `frame_cap` | an oversized reply and an oversized outgoing frame both answer `{error, frame_too_large}` and leave the fiber usable |
| `hot_swap` | loading the module again under a live kernel keeps every fiber active and every service and hook row intact |

The wire points need no python and no script file: the plugin half is a process of the same
VM speaking frames over a loopback socket, which is what makes the suite runnable anywhere
the release runs.

**Versioning.** The suite declares `TENON_KERNEL_CONTRACT=1` and base asks for exactly that
version; a suite that does not implement the requested version refuses instead of pretending.
The version is the *contract*, not the kernel: bug fixes, performance work and new optional
functions keep contract 1, while changing a frame, an existing function's meaning or a
lifecycle rule is contract 2 and needs a human to ship both halves (RFC section 10: "changing
the contract needs a human"). `tenon.erl`'s own version is whatever the release says; the
contract is what an upgrade is judged against.

## Deviations from NOTES section 9

1. **External unload terminates the plugin process.** Section 9.4 only describes this for
   `unmount`. We apply it to every unload (including a lost dependency) so that "the
   process is running" and "the fiber is loaded" are the same fact; `do_load` re-spawns
   the command when there is no port. The grace + `SIGKILL` backstop is unchanged. The
   alternative — keeping an idle process alive across a dependency outage — needs a
   second handshake to distinguish "unload" from "quit" and was not worth it.
2. **`svc` on a wire service returns the plugin's value directly**, not the internal
   `{rep, Value}` envelope. Errors surface as `{error, Reason}` / `{error, timeout}`.
3. **A `call` frame from a plugin uses an identity terminal** that returns the final
   (possibly rewritten) argument list, since the plugin cannot supply an Erlang fun.
4. **Extra exports** beyond the section 9 list: `start/0,1`, `start_link/0` and `stop/1`. `effect/2`
   also accepts `nil` from the body so Elixir plugins read naturally.
5. **Fibers stop when their kernel stops.** Fibers trap exits and are linked to the
   kernel, so a plain `EXIT` would have left them orphaned with dead ETS tables; they now
   stop with `shutdown` and close any port on the way out.
6. **`restart/1,2` return before an external plugin has re-settled.** The call would
   otherwise have to block the fiber on the wire. Call `status/1` afterwards; it is the
   settle point.
7. The root fiber has `module`, `id` and `parent` set to `undefined`, and `bail`/`get`
   report absence as `undefined` (not `nil`) — Erlang conventions, visible from Elixir.
8. As specified in 9.1, `parallel`, `serial`, the Service and Plugin macros, the Ctx
   struct and `update` are gone. `serial` was already identical to `bail` on the BEAM and
   `update(Config)` is `restart(Fiber, Config)`.
9. **A mounted socket's `{active, true}` is set by `mount/2`, not required beforehand.**
   The caller (a gateway) hands `mount/2` a socket that is `{packet, 4}, binary}` but may
   leave it passive; `mount/2` runs in the caller's own process — still the socket's real
   owner at that point — so it can transfer control to the new fiber and only then flip
   `active` on, both from the same process, with no window where a frame could reach
   the wrong mailbox. Handing `mount/2` an already-active socket also works as long as
   nothing has been sent to it yet, but is not required. A few of the smallest new
   dispatch functions (`connect_external/1`, `tx_send/2`, `close_port/1`'s socket clause)
   are written as one line per clause to keep `tenon.erl` under the 1000-line budget;
   every other new clause matches the file's existing multi-line style.
