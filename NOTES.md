# Tenon — working notes (single file, for human + AI review)

Tenon (榫): white-label functional port of the Cordis microkernel to Elixir/OTP. Reference: `deepseek-harness/vendor/cordis` (MIT). Repo `workspace/tenon`, app `:tenon`, modules `Tenon.*`.
Companion naming (per AGY, not yet applied): tenon (CLI) / tenon-core (Rust data plane) / tenon-term / tenon-browse / tenon-guard / tenon-flow / tenon-vault / tenon-canvas.

## 0. Decision log

- 2026-08-16 Pivot: integrate DeepSeek Harness (DSH). First item: Cordis kernel on Elixir.
- 2026-08-16 Not a 1:1 code port. Functional-level: same concepts/API semantics + same config-tree shape; do not load TS plugins. Reason: 1:1 port = rebuilding OTP on OTP; the only justification for Elixir is fault isolation, supervision, real multi-agent concurrency, hot code load, distribution.
- 2026-08-16 Label/permission/version/env "composability" stays OUT of the kernel. It is tree selection: `select(tree, labels) -> tree` in the loader, pure function.
- 2026-08-16 Bridge = plugin as external process over a standard wire (Erlang Port). Isolation level (none/landlock/bwrap/microVM/remote) is a launcher option, kernel unaware. VM-by-default deferred. Bus = BEAM native now; Broadway/Oban later if needed.
- 2026-08-16 Name: Tenon (was WL placeholder). Roles: Fable architecture+review, Opus 5 code, Sonnet 5 tests/exploration. Manual EVC, no strict process yet.
- 2026-08-16 `mix.lock` committed (Elixir convention, elixir.md rule overrides universal "no .lock").

## 1. Cordis -> Tenon mapping

| Cordis | Tenon | Note |
|---|---|---|
| Context (Proxy, prototype chain) | `%Tenon.Ctx{kernel, tables, fiber, parent}` | explicit `Ctx.get(ctx, :name)`; no Proxy |
| Fiber state machine | plugin process (GenServer under DynamicSupervisor) | pending/loading/active/failed/unloading/disposed |
| child dispose = effect on parent | child stop registered as parent effect + parent monitors child | parent unload cascades |
| effect disposer stack | disposer list in fiber state, run reverse in fiber process; Kernel monitors fibers, sweeps ETS on DOWN | kill-safe |
| inject / epoch | `inject/0`; Kernel recomputes epoch `[{name, provider}]` on provide/unprovide | missing -> unload; all present -> load; provider changed -> reload |
| Service provide | `Ctx.provide(ctx, name, impl)` = effect into ETS services | impl = pid / module / term |
| events emit/parallel/serial/bail/waterfall | `Tenon.Events`; dispatched in caller process, registration order; parallel via Task | waterfall: no `next` = short-circuit |
| internal/plugin,status,service | same atoms, emitted by fibers (never by Kernel) | |
| update(config) -> restart | same | |
| HMR | `:code.purge/load` + restart fiber (P2) | |
| schemastery | NimbleOptions (P2) | |
| isolate / intercept | later: one Registry/tables per realm | not v0.1 |
| plugin 3 shapes | one behaviour: `inject/0`, `apply/2` | |
| getTraceable this-rebinding, Symbol keys, declaration merging | not needed on BEAM | |

## 2. Config tree (P2)

`cordis.yml` shape: `{id, name, config, disabled, children}` + patch layers (replace/insert by id). `name` -> Elixir module or bridge spec. `!!js` unsupported; use overlays.

## 3. Bridge (P3)

Cordis has three channels: A direct service call, B event bus/waterfall, C JSON-RPC cross-process.
v0.1 external plugins use channel B only: `Tenon.Bridge` mounts a shell script via Erlang Port, JSON per line.
The script declares which events it listens to (and mode); Bridge registers hooks that forward `{event, args}`; for waterfall the script replies `{next: args}` or `{result: value}`. Script exit -> fiber failed; unload -> port closed.
`%{cmd, args, env}` only. protocol `:lines`, launcher `:port`. MCP/ACP/JSON-RPC, other launchers, VM: later.
Order: sh script echo demo -> DSH headless via the same lines protocol -> then channel A/C if needed.

## 4. Phases

- P0 done 2026-08-16: OTP 27.3 / Elixir 1.18.4 via mise (arm64 prebuilt), skeleton, gates green.
- P1 kernel (this note §6).
- P2 loader: yml tree + patch layers + ops (mount/unload/update/restart/dump) + hot code reload.
- P3 bridge: `:port`+`:lines` sh demo -> MCP -> DSH headless/ACP. Demo: supervise 2 external agents, hot-swap one Elixir policy plugin.
- P4: native seams (session log / tools / llm), Rustler term-core, more launchers, isolate.

Gates (every commit): `mix compile --warnings-as-errors && mix format --check-formatted && mix credo --strict && mix test`.
Rules: `../vibe-forge/rules-template/elixir.md`, `universal.md` (600 lines, no comments, no emoji).

## 5. P0 test scope

Foundation only: compile zero warnings, format, credo strict, one test (app starts, `Tenon.Supervisor` alive). Behavior tests start at P1.

## 6. P1 design — kernel

### 6.1 Modules (lib/tenon/)

| Module | Role | ~LoC |
|---|---|---|
| `Tenon.Plugin` | behaviour: `@callback inject() :: [atom]` (optional, default `[]`), `@callback apply(Ctx.t, config) :: :ok \| {:error, term}` | 20 |
| `Tenon.Ctx` | struct + facade: `plugin/4 effect/2 on/4 emit/3 parallel/3 serial/3 bail/3 waterfall/4 get/2 provide/3` | 80 |
| `Tenon.Kernel` | GenServer per kernel instance; owns ETS (`fibers`, `services`, `hooks`, `seq`) + DynamicSupervisor; monitors fibers; dependency/epoch bookkeeping; `start_link/1 root/1 tree/1` | 150 |
| `Tenon.Fiber` | GenServer per plugin instance: load/unload/update/restart, disposer stack, children, status row | 200 |
| `Tenon.Events` | hook table ops + 5 dispatch modes | 120 |
| `Tenon.Service` | `provide/3` + `use Tenon.Service, name:` sugar (`start/2 -> {:ok, impl}`) | 40 |

Target <= ~650 LoC total, tests separate.

### 6.2 Invariants (design rules, enforce in review)

1. Kernel never calls a fiber synchronously and never runs user code (no apply, no hooks, no disposers). Kernel -> fiber is cast only. Fiber -> Kernel may be call.
2. Kernel emits no events. Fibers emit `internal/*` about themselves.
3. Dispatch runs in the caller process, hooks in registration order (prepend goes before). `emit`: each hook isolated (rescue + Logger, never breaks emitter). `parallel`: Task.async_stream, returns `:ok | {:error, [reason]}`. `serial`/`bail`: first non-nil result wins, else nil (bail == serial on BEAM). `waterfall(ctx, event, args, terminal)`: hook `fn ..args, next -> ... end`; `next.(args...)` passes possibly modified args downstream; not calling `next` short-circuits; terminal is the default. Coder must verify call order against `vendor/cordis/src/events.ts:234-243`.
4. Every registration returns a disposer. `Ctx.effect(ctx, fun)`: fun runs now in the caller, returns disposer or nil; the disposer is handed to the owning fiber: from another process via `GenServer.call(fiber, {:effect, d})`, from inside the fiber process itself (during apply) via `send(self(), {:effect, d})` (mailbox FIFO keeps order, no deadlock, no process dictionary). Disposers run reverse order in the fiber process on unload.
5. Hooks and services rows carry owner fiber pid. Kernel monitors every fiber; on DOWN it deletes rows by owner and re-evaluates dependents. No orphan registrations after kill.
6. apply/2 raising -> fiber `:failed` (process stays, error kept, shows in tree; `restart` retries). Any other crash -> DOWN sweep.
7. inject epoch: `[{name, provider_pid}]`. pending + all present -> load. active + one missing -> unload -> pending. active + provider pid changed -> reload. Optional deps: `Ctx.get/2` returns nil.
8. Child fiber: `Ctx.plugin(ctx, mod, config, opts)` from a fiber -> Kernel starts child with parent=caller; parent gets an effect that stops the child; parent monitors child. Parent unload -> children disposed first (reverse effect order).
9. Multiple kernels per VM (tests use one kernel per test). Ctx carries kernel pid + table refs; ETS tables unnamed.
10. Fiber ids: monotonic uid per kernel; optional caller `id` (string) for P2 loader.

### 6.3 Status row (ETS `fibers`, single writer = the fiber)

`{pid, uid, id, parent, module, status, inject, epoch, error}`; `Kernel.tree/1` returns nested maps.

### 6.4 Tests (Sonnet, adversarial)

load/unload; disposer reverse order; `Process.exit(fiber, :kill)` leaves no hooks/services rows; inject waits then loads; provider dispose -> dependent pending; provider back -> reload; provider swap -> reload; 5 modes incl. prepend and waterfall short-circuit + arg rewrite; parent unload disposes children first; apply raise -> failed, restart -> active; internal/* observed; two kernels do not see each other.

## 7. Open questions & Architectural decisions

- **Q1: Should `emit` swallow hook errors (Cordis propagates)?**
  * **Decision**: **Confirmed isolation (`rescue + Logger.error`)**. `emit` is a decoupled broadcast notification; individual observer failures must never crash the business caller. Errors in `waterfall` / `serial` will propagate.
- **Q2: Effect collection during `apply/2` (pdict accumulator vs explicit messaging)?**
  * **Decision**: **Avoid hidden process-dictionary**. Use `Ctx.effect(ctx, fun)` via explicit Fiber GenServer message/state management or return `:ok | {:ok, disposer}` from `apply/2` to preserve OTP purity and debuggability.
  * **Applied**: invariant 6.2.4 — disposer travels as a message to the fiber (`call` from other processes, `send` to self during apply). `apply/2` may also return `{:ok, disposer}`.
- **Q3: Fiber process lifecycle on `:failed` (stay alive vs death + tombstone)?**
  * **Decision**: **Stay alive in `:failed` state**. Retains full inspectability in `Kernel.tree/1`, preserves the error stack trace, and allows clean manual/supervised retry via `Fiber.restart/1`.

### Applied in P1 (2026-08-16)

- Effect transport: registration from another process is `GenServer.call(fiber, {:effect, ref, d})`, from inside the fiber it is `send(self(), ...)`. Calling a disposer from inside its own fiber is likewise deferred to the next callback (a fiber cannot call itself); everywhere else it is a call. No process dictionary.
- `Fiber.status/1` recomputes the epoch before replying, so it doubles as the settle point: kernel -> fiber stays cast-only, callers pull. `Ctx.plugin/4` uses it and returns only after the child settled.
- Kernel waits on `DynamicSupervisor.start_child` (fiber `init/1` only, no user code, no kernel call); loading happens in `handle_continue`, so the kernel never calls a running fiber.
- Parent unload runs disposers strictly in reverse effect order. A child is disposed at the position where it was mounted, not unconditionally first; effects registered after the child are disposed before it.
- `apply/2` returning `{:error, reason}` or an unexpected shape fails the fiber exactly like a raise. Effects collected before the failure are kept and run on the next unload/restart.
- Providing a name that is already live raises `ArgumentError` (Cordis throws).
- Plugins `use Tenon.Plugin`, which imports `Kernel` except `apply/2` (BEAM name clash).
- Kernel LoC came out at 765 (ctx 91, kernel 163, fiber 292, events 118, plugin 36, service 65), above the ~650 estimate; every file is well under the 600 limit.
- `mix.exs` gained `elixirc_paths` so `test/support/plugins.ex` compiles in the test env.

## 8. P1 result (2026-08-16)

Commit `6ddb254` + fix. lib/tenon: plugin 36, ctx 91, kernel 163, fiber ~300, events 118, service 65 (~770 LoC). 51 tests (34 coder + 17 adversarial), 0 failures, stable across seeds 1/2/7/12/19/33.
Adversarial findings: (1) FIXED root fiber not settled before `Kernel.init` returned -> `tree/1` flaky `:pending`; root (module nil) now mounts inside `init`. (2) `internal/plugin` fires on mount only, not dispose (documented, not symmetric). (3) provider withdraw deletes ETS row before dependents unload; dependents never see stale impl but unload is asynchronous vs Cordis's synchronous cascade. (4) a hook calling its own fiber synchronously gets `{:calling_self, _}` swallowed by `emit` isolation (logged, no hang). Constraint: hooks must not call fibers synchronously.

## 9. Atom kernel spec (decision 2026-08-16, supersedes §6 layout)

Goal: one Erlang module `kernel/src/tenon.erl` (< 1000 LoC, target ~800), one ExUnit test file `kernel/test/tenon_test.exs`, one `kernel/README.md`. Zero deps (OTP 27 `json` module). Everything else (loader, yml, config schema, SDKs, bridges to brokers) is a plugin outside the kernel. Elixir P1 (`lib/tenon/*`) is retired; its semantics and adversarial tests are the acceptance list.

### 9.1 Keep / cut

Keep: plugin+service+hook registry (ETS), lifecycle (mount/unmount/restart, inject gating, cascade on death), dispatch `emit` + `call` (waterfall) + `bail` (first non-null), effects with reverse-order disposal, `internal/plugin|status|service`, `tree`, multi-kernel per node, wire for external plugins, `code_change`.
Cut: `parallel`, `serial` (= bail), Service sugar, Plugin `use` macro, Ctx struct (ctx is a map), `update` (= `restart(Fiber, Config)`).

### 9.2 API (Erlang, in-VM plugins; Elixir calls it directly)

- `tenon:start_link(Opts) -> {ok, K}`; `tenon:root(K) -> Ctx`; `tenon:tree(K)`; `tenon:status(Fiber)`.
- Ctx = `#{kernel := pid(), tabs := map(), fiber := pid()}`.
- `tenon:mount(Ctx, Spec) -> {ok, Fiber}`; Spec `#{module := M, config => C, id => Id}` or `#{cmd := Cmd, args => [..], env => [..], config => C, id => Id}` (external, Port). Child of Ctx fiber; settled before return.
- `tenon:unmount(Fiber)`, `tenon:restart(Fiber)`, `tenon:restart(Fiber, Config)`.
- `tenon:effect(Ctx, Fun) -> Disposer`; `tenon:on(Ctx, Event, Fun) / on(Ctx, Event, Fun, #{prepend => bool})` -> Disposer.
- `tenon:emit(Ctx, Event, Args) -> ok` (hooks isolated, errors logged). `tenon:call(Ctx, Event, Args, Terminal) -> Result` (waterfall, hook `fun(A.., Next)`, `Next(A'..)` rewrites args, no Next = short-circuit; errors propagate). `tenon:bail(Ctx, Event, Args) -> Result | undefined`.
- `tenon:provide(Ctx, Name, Impl) -> Disposer` (duplicate raises); `tenon:get(Ctx, Name) -> Impl | undefined`; `tenon:svc(Ctx, Name, Method, Args) -> Result` (Impl module -> `Impl:Method(Args..)`; fun -> `Impl(Method, Args)`; wire ref -> request to owning port).
- Plugin module callbacks: `inject() -> [atom()]` (optional), `load(Ctx, Config) -> ok | {ok, Disposer} | {error, R}`.

### 9.3 Process model

- Kernel gen_server: mount/unmount, monitors fibers (trap_exit + start_link), DOWN sweep of hook/service/fiber rows by owner, inject re-evaluation via cast `refresh`. Never calls a fiber synchronously, never runs user code, never dispatches. Not in any hot path.
- Fiber gen_server (same module, own state record): load/unload in-process, disposer stack, own status row. External fiber additionally owns the Port and a pending-request map; it never blocks on the wire.
- Dispatch runs in the caller process straight from ETS (`hooks` ordered_set keyed `{Event, Seq}`, prepend = negative seq; `read_concurrency`). Wire-originated `emit`/`call` from a plugin is executed in a spawned worker, never in the port-owner fiber (no self-wait deadlock).
- Root fiber (no module) mounts synchronously in `init`.

### 9.4 Wire (external plugins)

Transport: Erlang Port, `{packet, 4}`, binary, payload JSON (OTP `json`); ETF codec later as one clause. Socket transport later, same frames.
Kernel -> plugin: `load{config}`, `unload`, `hook{req,event,args,mode: emit|call}`, `result{req,result}` (only if plugin asked `await`), `svc{req,method,args}`, `rep{id,result|error}`.
Plugin -> kernel: `hello{inject:[..]}` (first frame), `on{hook,event,prepend}`, `off{hook}`, `provide{name}`, `unprovide{name}`, `emit{event,args}`, `call{id,event,args}`, `svc{id,name,method,args}`, `next{req,args,await}`, `rep{req,result|error}`.
Rules: every request has a deadline (default 30 s, per-kernel option) -> `{error, timeout}`; plugin exit -> fiber `failed` with exit status, rows swept; unmount -> `unload` then port close (kill after grace 5 s). Control plane only: bulk data (PTY bytes, DOM) goes plugin-to-plugin over their own channel; kernel brokers discovery via services.

### 9.5 Hot swap

Single module: `c:l(tenon)` swaps kernel + all fibers atomically; state lives in ETS, records versioned in `code_change/3`. In-VM plugin code: `l(Mod)` then `tenon:restart(Fiber)`. External plugin: `restart` re-spawns the command. README documents the procedure.

### 9.6 Acceptance

All P1 tests translated (load/unload, reverse disposers, kill sweep, inject wait/lose/regain/swap, prepend, waterfall rewrite/short-circuit, cascade, failed->restart, internal events, two kernels, 500-fiber stress no leak) + wire: python3 test plugin (hello/on/provide/call/next+await/svc, exit -> failed, unmount closes port) + perf smoke (100k emit with 3 hooks, 10k wire round trips; assert generous bounds, print numbers).
Gates: erlc warnings as errors, `mix format --check-formatted` (test), `mix test`. LoC: `wc -l src/tenon.erl` < 1000.

## 10. Atom kernel result (2026-08-16, commit fadd4be)

`kernel/src/tenon.erl` 929 LoC, zero comments, zero deps. Tests: `tenon_test.exs` (35, spec + wire + perf) and `tenon_adversarial_test.exs` (18, hot swap / scale / concurrency / wire abuse); 53 green, seeds 1-5, no flakiness. README 305 lines carries the explanation.
Verified: double in-place `code:load_file(tenon)` with live fibers + external plugin, all keeps working; 100k emit x 3 hooks ~100 ms (~1M/s); 10k wire round trips (python3) ~350-390 ms (~25-29k/s); 10k-row hooks table costs 1.3x empty (partial-key select, no scan); 20k-frame flood no leak; 50 procs x 20 mount/unmount concurrent, tables back to baseline.
Defects found by review/adversarial and fixed: unguarded `json:decode` crashed fiber; `status/1` respawned a failed external plugin once (epoch stayed inactive); kernel dies with its `start_link` caller (documented; `start/1` unlinked added; ETS writes tolerate dead table). Earlier audit fixed 7 more (orphan fibers on kernel death, svc envelope leak, pid-recycle kill, unmount grace stall, reload not re-sending load, dead parent disposer entries, stale wire map).
Known/accepted: `notify` is O(total fibers) per provide/unprovide (0.5 s per 100 cycles at 10k fibers); external unload = process exit + respawn on load; wire atoms via `binary_to_atom` (trusted control plane); hooks must not call their own fiber synchronously.
Not in kernel (by design, next work): language SDKs (py/ts/rust ~100 lines each), loader plugin (yml tree + patch), socket transport + ETF codec, isolate realms, broker bridge.

## 11. Arch note for later: message-only wire, big payloads (2026-08-16)

Kernel is already registrar + lifecycle + dispatcher; wire frames are async messages with req-id correlation; in-VM `call/svc` are Erlang-idiomatic sync views of the same messages. Not refactoring that.
Rules to adopt now: (1) frame size cap (default 1 MB, kernel option) -> `{error, frame_too_large}`; (2) bulk data never crosses the wire: plugins return handles (path / UDS / URL / fd / stream endpoint) and talk plugin-to-plugin; kernel only brokers discovery. (3) wire v1.1: Port `nouse_stdio` (plugin reads fd 3, writes fd 4), stdout/stderr free for logs, frames unchanged. (4) wire v2 later: UDS/TCP transport, same frames, enables remote plugins/nodes.
Deferred: fully async in-VM API, kernel-side streaming.

## 12. P2 plan: config model + DSH compatibility

Compat levels: L1 config files (cordis.yml, cordis.patch.yml, profiles/bundles) accepted unchanged -> same tree. L2 DSH TS plugins run unmodified -> the whole DSH plugin tree runs in one Node process on real Cordis, mounted as one Tenon external plugin (`bridge/dsh`). L3 selected DSH services/events mirrored onto the Tenon bus via a manifest. Per-plugin Tenon fibers for TS plugins: deferred (would mean re-implementing Cordis semantics in TS).

Layout:
```
tenon/
  kernel/        atom kernel (done)
  loader/        Elixir in-VM plugin: yml tree + patch layers + profile/bundle resolution + name registry -> mount specs; ops (reload/dump); golden test vs `dsh --dump-config`
  bridge/dsh/    TS: Cordis plugin `tenon-bridge` (inside the DSH Node process, mirrors services/events over the wire) + `tenon-dsh-host` launcher (runs a DSH profile with the bridge patched in, wire on fd 3/4)
  sdk/py sdk/ts sdk/rs   wire SDKs (~100 lines each): frame io, hello/on/provide/svc, handle-based bulk data
  playground/    examples (gitignored now; promote the good ones to examples/ later)
```
Steps:
- P2.0 wire v1.1 (kernel): `nouse_stdio`, frame cap, README + playground plugins updated. Small.
- P2.1 sdk/py + sdk/ts (needed by bridge and tests); sdk/rs after.
- P2.2 loader (Elixir): parse yml tree; patch layers by id (replace/insert/disable); name registry `name -> {module | cmd | dsh}`; `dsh-*` rows collapse into one bridge/dsh mount; `!!js` passed through untouched to the DSH host; ops reload/dump; tests: golden vs `dsh --profile headless --dump-config` (Sonnet explores DSH app-boot composition rules first).
- P2.3 bridge/dsh: explore how DSH profiles install out-of-tree plugins (`cordis.patch.yml`, profile home); `tenon-bridge` Cordis plugin: manifest of mirrored services/events; `tenon-dsh-host` bin; demo: Tenon mounts DSH headless, a python plugin registers a `tools/pre-execute` guard visible to DSH tools, and calls `ctx.llm` via svc.
- Language decisions: loader Elixir (control plane, direct API); bridge TS (must); Rust only later for a CLI launcher around `mix release`.

## 13. P2.0-2.2 results (2026-08-16)

- P2.0 `d9993d0` wire v1.1: Port `nouse_stdio` (fd 3 in / fd 4 out), frame cap `max_frame` (option > env `TENON_MAX_FRAME` > 1 MB) enforced both directions, cap + deadline passed to plugin env. kernel 978 LoC, 58 tests.
- P2.1 `88e93bf` sdk/py/tenon.py 282, sdk/ts/tenon.ts 298 (zero deps, re-entrant waits, next/await, svc), same demo in both, sdk/test 8 conformance tests incl. py<->ts nested svc. Finding: Node `fs.createReadStream(fd 3)` blocks exit; use `net.Socket({fd:3})`.
- P2.2 `5f64588` loader/ (Elixir, 753 LoC): faithful `applyEntryPatches`, DSH layer order, `!!js` capture, stable ids `anon:<parent>/<name>:<n>`, diff reload (mount/unmount/restart/toggle), groups cascade, collapse mechanism for dsh rows, reload/dump; 49 tests.
- Small follow-ups: (a) kernel README should document + test mounting into another fiber's ctx from a foreign process (loader groups rely on it); (b) SDK-side oversize error is `{:error, "frame_too_large"}` (binary) vs kernel atom — normalize later; (c) `notify` O(N) index later.
- Next P2.3: DSH pnpm install, custom `tenon` profile = dsh-base + `tenon-bridge` row, bridge mirrors services/events over the wire via sdk/ts.

## 14. P2.3 result: DSH runs as one Tenon plugin (2026-08-16, commit 71af016)

`bridge/dsh`: Cordis plugin `tenon-bridge` (TS, reuses sdk/ts) inserted into a custom DSH profile `tenon` (bundle dsh-base + one patch row). DSH boots unmodified as ONE external fiber (`node apps/cli/lib/bin.js --profile tenon`, fd 3/4) in ~1.1 s. Service `dsh`: ping, pid, mirrors, tools.list, tools.execute, sessions.list/create, agents.list. Mirrors: `session/created`, `session/event` (emit), `tools/pre-execute` (call, JSON projection + pick allowlist; `{deny}` short-circuits DSH's tool pipeline).
Proven end to end: a sdk/py plugin on the Tenon bus denies a DSH tool call (`rm -rf` -> denied with the python reason; `echo` -> ok) — no model turn needed. 5 tests green.
Compat status: L1 config files (loader) yes; L2 DSH TS plugins unmodified yes (real Cordis inside Node); L3 selected services/events on the Tenon bus yes (manifest-driven). Deviations in bridge/dsh/README.md.
Prereq for the built launcher: DSH `pnpm install`, `pnpm run build:lib:host` and `build:lib:client`. Loader-side collapse of dsh rows into the profile patch (writing `$DSH_HOME/profiles/tenon/cordis.patch.yml`) is the remaining L1 glue: P2.4.

P2.4 done (commit 5c44981): built-in collapse target `Tenon.Loader.Dsh` writes `$DSH_HOME/profiles/<name>/{package.json,cordis.patch.yml}` (bridge row + harvested rows, `!!js` re-emitted) and mounts DSH as one fiber; reload is restart-free (DSH hot-reloads the profile patch), loader 64 tests + `bridge/dsh/test` 6.
