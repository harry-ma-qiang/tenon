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

## 15. Status 2026-08-16 (push)

Per project, source lines (`wc -l`, no tests) and green tests:

| Project | LoC | Tests |
|---|---|---|
| `kernel/src/tenon.erl` | 989 | 61 (`tenon_test.exs` 43, `tenon_adversarial_test.exs` 18) |
| `loader/lib` | 1023 (config 347, tree 404, dsh 155, loader 46, server 51, group 9, application 11) | 64 |
| `cli/lib` | 295 (cli 169, registry 84, signals 42) | 6 |
| `sdk/py/tenon.py` | 282 | 16 in `sdk/test` (py + ts + rs + plugins/term) |
| `sdk/ts/tenon.ts` | 298 | same suite |
| `sdk/rs/src/lib.rs` | 547 | same suite |
| `plugins/term/src/main.rs` | 297 | same suite |
| `bridge/dsh/src` | 460 (mirror 239, plugin 221) | 6 (`bridge_test.exs` 5, `dsh_loader_test.exs` 1) |

153 tests total, all green. Gates green everywhere: kernel/loader/cli `mix compile` +
`format --check-formatted` + `credo --strict` (loader, cli) + `mix test`; `sdk/test` and
`bridge/dsh/test` `mix test`; `sdk/rs` and `plugins/term` `cargo build --release` +
`clippy --all-targets -D warnings` + `fmt --check`; `sdk/ts` and `bridge/dsh` `tsc --noEmit`.

Perf re-measured today (OTP 27.3, arm64): 100k emit x 3 hooks 107 ms (~930 k/s); 10k wire
round trips to python3 353 ms (~28 k/s); 100k emit against a 10 003-row hooks table 131 ms
vs 100 ms for a 3-row table; 100 provide/unprovide cycles over 10 001 fibers 176 ms.

Compat status unchanged from §14: L1 config files yes (loader), L2 DSH TS plugins
unmodified yes (real Cordis in one Node process, one fiber), L3 selected services/events on
the Tenon bus yes (manifest-driven). Deviations per README.

New in this pass: `cli/` — escript `tenon` with `start` (mount + stay alive, SIGHUP reload,
SIGTERM graceful unmount), `dump` and `check` (compose only, exit 1 on a bad row), and a
name registry from `.yml` / `.exs` / a module. SIGINT is not routable via `os:set_signal/2`,
so Ctrl-C aborts the VM and plugins exit on wire EOF. `Cargo.lock` is now committed for
`sdk/rs` and `plugins/term` (bin crates, mirrors the `mix.lock` decision of §0). Root
README, AGENTS.md and every subproject README swept for stale numbers and terminology.

§13 follow-ups are closed: (a) cross-fiber mount is invariant 10 of the kernel README and
tested; (b) SDK oversize error is normalized at the kernel boundary to the atom
`{:error, :frame_too_large}`; (c) `notify` uses the `deps` index, no fibers-table scan.

Deferred, in rough order of value: wire v2 socket transport (UDS/TCP, same frames, remote
plugins and nodes); ETF codec as one extra clause next to JSON; isolate realms (one
registry/table set per realm) for `intercept` / `isolate`; graceful kernel stop (unmount the
tree before closing ports, so `stop/1` is quiet with live external plugins); loader file
watch for native rows (today `reload/1` is explicit; only DSH rows hot-reload through the
profile patch); `mix release` packaging of kernel + loader + cli as one artifact.

## 16. Live run: DSH web under the Tenon kernel (2026-08-16)

`cli/tenon start playground/web/tenon.yml --dsh-bundles dsh-base,dsh-web-app ...` (kernel = top process; DSH node process is its child via erl_child_setup). Tree: guard (py, sdk/py, `tools/pre-execute` call hook, prepend) / audit (py, emit hooks -> audit.jsonl) / dsh (collapsed, web at http://127.0.0.1:3080). Real model deepseek-v4-flash via DEEPSEEK_API_KEY (env only). Smoke via JSON-RPC `/api/*`: pong PASS; bash `echo tenon-ok` executed PASS; `rm -rf` denied by the Tenon-side python guard PASS; audit lines PASS. Fix landed: `--dsh-bundles` flag (`618809c`). Test-profile rows `sandbox-policy danger-full-access` + `approval never` (no sandbox backend on this host) live in the Tenon yml as `tenon: dsh` rows and were hot-applied with SIGHUP (DSH HMR, same pid). Caveat unchanged: TS plugins inside Node run on real Cordis (L2 design). Web binds 127.0.0.1 only; use an SSH tunnel.

## 17. P3.0 result: the safety floor (2026-08-18)

RFC `RFC-P3-minimal-harness.md` step P3.0: one Rust binary that boots and watches a BEAM
guardian node and a root environment node, a UDS front door shared by nodes and CLI, LKG with
`reset`, and `kill -9` of base taking every node down. Base runs no plugin code.

### What was built

`beam/` — mix project `tenon_beam`, release `tenon_beam` with ERTS included (self-contained,
21 MB gzipped, 68 MB when embedded in the binary). Depends on `../kernel` and `../loader`.
`Tenon.Beam.Boot` turns `TENON_ROLE` / `TENON_ENV` / `TENON_BASE_SOCK` / `TENON_PROFILE` into
one kernel with the loader on the profile plus two barebone plugins mounted outside the
loader: `Tenon.Beam.Link` (outbound UDS to base, `node.register`, answers `health` / `tree` /
`reload`, publishes the `link` service, and stops the node when the socket closes) and
`Tenon.Beam.Guardian` (probes base for the agent env's health every `interval`, sends
`reset{env}` after N consecutive failures). `RELEASE_DISTRIBUTION=none` and
`RELEASE_MODE=embedded`: no distribution, no epmd, no code loading at runtime.

`rs/` — Cargo workspace, one bin target `tenon`. `base` (home layout, config, frame codec,
peer with per-direction request correlation, node spawn/terminate, the supervisor actor, the
UDS server, the release payload extractor, the CLI client), `storage` (`state.sqlite`, WAL +
`synchronous=NORMAL` + `busy_timeout=5000`, `events` append-only, `envs`), `sandbox` (trait +
`none` backend, already on the boot path), `harness` and `worker` (role stubs, exit 2), `cli`
(clap subcommands `start|attach|stop|reset|status|harness|worker` and the `build.rs` that
`include_bytes!`es a release tarball named by `TENON_RELEASE_TAR`).

### Lines and tests

| Part | LoC | Tests |
|---|---|---|
| `beam/lib` | 524 (link/server 154, boot 83, guardian/server 78, link/handlers 56, registry 48, frame 37, link 30, guardian 20, application 18) | 14 (`link_test.exs` 9, `guardian_test.exs` 5) against a fake UDS base |
| `rs/base/src` | 1568 (base 436, home 245, lib 230, server 173, node 128, config 103, peer 82, release 74, client 50, frame 47) | covered by the integration suite |
| `rs/storage` | 222 (incl. 3 unit tests) | 3 |
| `rs/sandbox` | 115 (incl. 2 unit tests) | 2 |
| `rs/cli` | 133 (main 109, build.rs 24) | 7 in `cli/tests/boot.rs` |
| `rs/harness`, `rs/worker` | 30 | 1 each |

Rust: 2068 source lines (135 of them inline `cfg(test)`) plus 294 lines of integration test.
BEAM: 524 source lines plus 262 of test. 14 Elixir tests and 14 Rust tests, all green.

Gates, full last lines: beam `mix compile --warnings-as-errors` clean, `mix format
--check-formatted` clean, `mix credo --strict` "110 mods/funs, found no issues",
`mix test` "14 tests, 0 failures", `MIX_ENV=prod mix release` assembled. rs `cargo build
--release` Finished, `cargo clippy --all-targets -- -D warnings` Finished with no output,
`cargo fmt --check` exit 0, `cargo test` 7 + 3 + 2 + 1 + 1 = 14 passed, 0 failed.

### Measured on this box (OTP 27.3, arm64, 4 cores)

- `tenon start` to both nodes registered: **1.2 s** (embedded payload, first-run extraction
  of the 21 MB tarball included).
- `kill -9` base to both nodes exited: **1.1-1.5 s**, well inside the 5 s the gate asks for.
  No OS supervision involved: the nodes see their socket close and stop themselves.
- `tenon reset`: the env node gets a new pid, its LKG profile is restored, the guardian's pid
  is unchanged. `SIGKILL` of the env node: base restarts it and `status` shows `restarts: 1`.

### Deviations from the RFC

Full list with reasoning in `rs/README.md` and `beam/README.md`. The load-bearing ones:

1. No `rollback` / `approve` / `run` subcommand and no `wire` crate; `sdk/rs` stays where it
   is and is only a path dependency of `worker` (whose public error type it supplies). Base
   speaks the wire frames over a socket, not fd 3/4, so it needs no SDK.
2. Only `events` and `envs` in `state.sqlite`. The other section-9 tables arrive with the
   phases that write them (P3.2-P3.4) rather than as frozen empty schemas.
3. The guardian's release directory is not `chmod`ed read-only: G and A run from the same
   extracted release. Read-only is enforced by embedded mode, no distribution, and base never
   writing under `erts/`.
4. `reset` answers when the old node is dead and the new one is spawned, not when it has
   registered, so the answer stays inside the requesting node's deadline.
5. `--exit-on-detach` counts `subscribe`rs, so `tenon status` and `tenon stop` cannot trip it;
   only `tenon attach` holds the door open.
6. The sandbox trait is already on the boot path with the `none` backend (one instance per
   env), so P3.1 replaces a backend instead of adding a seam.

### Next

P3.1: sandbox trait with oci and landlock backends, the conformance suite, the kernel
socket-backed external fiber spec and the gateway plugin in node A.

## 18. P3.1a result: kernel socket-backed fibers + gateway plugin (2026-08-18)

`kernel/src/tenon.erl`: 989 -> 997 lines (+8 net; 42 insertions/34 deletions). A transport
tag `{port, Port} | {socket, Sock} | undefined` replaces the bare `Port` field so
send/close/`handle_info` share one code path for a spawned OS process and an
already-connected socket; `kind/1` treats a `socket` key exactly like `cmd` (both are
`external`); `mount/2` claims a passed socket's controlling process itself. New:
`kernel/test/tenon_socket_test.exs` (192 lines, 5 tests). `beam/lib/tenon/beam/gateway.ex`
(23 lines) + `gateway/server.ex` (121 lines) = 144 lines; `beam/test/gateway_test.exs` (108
lines, 4 tests); `beam/lib/tenon/beam/boot.ex` +25 lines (mounts `Gateway` in agent-role
nodes, alongside `Link` and the guardian-role `Guardian`).

### Lines and tests

| Part | LoC | Tests |
|---|---|---|
| `kernel/src/tenon.erl` | 997 (was 989, +8) | 66 (was 61; +5 socket tests) |
| `beam/lib/tenon/beam/gateway*` | 144 (`gateway.ex` 23, `gateway/server.ex` 121) | 4 (`gateway_test.exs`) |
| `beam/lib/tenon/beam/boot.ex` | 108 (was 83, +25) | exercised by `gateway_test.exs` + existing `link`/`guardian` tests |

Gates, full last lines: kernel `mix compile` clean (erlc `warnings_as_errors`), `mix format
--check-formatted` clean, `mix test` "66 tests, 0 failures". beam `mix compile
--warnings-as-errors` clean, every file this task touched formatted (`test/link_test.exs`
already fails `mix format --check-formatted` on `main`, untouched here — see deviation 5),
`mix credo --strict` "139 mods/funs, found no issues", `mix test` "18 tests, 0 failures",
`MIX_ENV=prod mix release` assembled.

### Deviations

1. **`mount(Ctx, %{socket => Sock})` does not require `Sock` to already be `{active,
   true}`.** `mount/2` runs in the caller's own process — the socket's real owner at that
   point — so it transfers control to the new fiber and only then flips `active` on, both
   from that one process. Activating before the transfer is a race OTP's own docs call out:
   a message can be delivered to the old owner in the gap. `Tenon.Beam.Link.Server` already
   uses this exact sequence for the base connection; the gateway's acceptor mirrors it.
2. A handful of the smallest new kernel dispatch clauses (`connect_external/1`, `tx_send/2`,
   the socket clause of `close_port/1`, several `handle_info` heads) are one line per clause
   to keep `tenon.erl` under the 1000-line ceiling; every other new clause matches the
   file's existing multi-line style. Two pre-existing clauses (`close_port`'s port case, the
   frame-cap check now factored into `handle_incoming/2`) were reformatted onto fewer lines
   for the same reason — no logic changed, confirmed by the full test suite before and after.
3. **Restarting a socket fiber fails it with `socket_unavailable`** instead of trying to
   reopen the socket — `restart/1,2`, and a refresh after a lost-then-regained dependency,
   go through `spawn_or_load/1`, which has nothing to respawn a closed socket with, per the
   RFC. The very first mount is unaffected: it is dispatched through a separate
   `connect_external/1`, not through the restart path.
4. **The gateway's acceptor hands each accepted socket to a short-lived process before
   calling `:tenon.mount/2`**, rather than mounting inline in the accept loop. `mount/2`
   blocks its caller until the fiber settles (`hello` arrives, or the deadline fires), so
   mounting inline would let one slow or silent client stall every other connection from
   being accepted.
5. A pre-existing, low-frequency (roughly 1 in 3-5 runs, reproduces identically on
   unmodified `tenon.erl` from `main`) benign crash log — `ets:select` on a hooks table that
   no longer exists, from a fiber processing a stray exit message after its kernel has
   already torn down during test cleanup — surfaces in both the kernel and beam suites. It
   never fails a test and is not introduced by this work; left alone as out of scope.
   `beam/test/link_test.exs` being unformatted on `main` is the same kind of pre-existing,
   out-of-scope condition, left untouched rather than reformatted as a drive-by fix.

### Next

P3.1 remainder: the sandbox trait with `oci` and `landlock` backends and the conformance
suite (the other half of the P3.1 row). The worker as a resident process is P3.2.

## 19. P3.0 adversarial: 4 defects fixed (2026-08-18)

An uncommitted adversarial suite (`rs/cli/tests/adversarial/`, 19 tests) found four defects in
the P3.0 Rust base; all four are fixed, and one Elixir line changed to carry a token.

- Double start not refused: `run/base.lock` holds an exclusive `flock` for the base's
  lifetime; a second `start` prints `already running (pid N)` and exits non-zero without
  touching the running instance; a crashed base's lock has no holder so the next start takes
  it over and cleans stale `run/base.sock` / `run/base.ready`; `run/base.ready` is now written
  atomically (temp file + rename).
- SIGTERM/SIGINT during boot orphaned nodes: signal handlers are installed before any node is
  spawned; a signal mid-boot kills already-spawned nodes with a short grace (300 ms, shorter
  than `stop_grace_ms` since an unregistered node has nothing to protect) then SIGKILL, and
  removes `run/` files before the process exits.
- `reset` never restored `state.sqlite`: at boot and at every `reset`, base runs `PRAGMA
  integrity_check` (unopenable or zero-length also counts as corrupt); a corrupt file is
  replaced from `lkg/state.sqlite` and a `state.restored` event is logged; a healthy file is
  left untouched so recent events are never discarded.
- Unauthenticated `node.register`: base generates a random 32-byte token per spawned node in
  `TENON_NODE_TOKEN`; `node.register` must carry that token and the exact spawned pid or base
  rejects it and logs `node.register_rejected`; `Tenon.Beam.Link.Server` now sends the token
  from its own environment (one line); a forged registration from the CLI socket always fails.

Gates, full last lines: rs `cargo build --release` Finished, `cargo clippy --all-targets -- -D
warnings` Finished with no output, `cargo fmt --check` exit 0, `cargo test` run three times,
stable: 1 (token) + 19 (adversarial) + 7 (boot.rs) + 7 (storage/sandbox/harness/worker unit)
= 34 passed, 0 failed each run. beam `mix compile --warnings-as-errors` clean, `mix credo
--strict` "110 mods/funs, found no issues", `mix test` "14 tests, 0 failures", `MIX_ENV=prod
mix release` assembled. `mix format --check-formatted` still fails only on the pre-existing,
out-of-scope `beam/test/link_test.exs` noted in §18.5 — untouched here.

Deviation: `rs/base/Cargo.toml` gained a direct `rusqlite` dependency (already resolved via
`tenon-storage` in the workspace lock) so the boot/reset integrity check can run `PRAGMA
integrity_check` from `rs/base/src/integrity.rs`.

## 20. P3.1b result: sandbox oci/landlock backends, base wiring, gateway gate (2026-08-18)

The other half of the P3.1 row: real `oci` and `landlock` `Sandbox` backends, base owning
one instance per env end to end (spawn at boot, destroy+recreate at reset, destroy at
stop), a conformance suite over both, and the P3.1 gate itself — a python plugin started
inside an oci sandbox via `sandbox.exec`, registering through the gateway, answering a
`svc` round trip proxied by a small addition to `Tenon.Beam.Link`.

Both real backends ran here: `podman` 4.9.3 and `docker` 29.4.0 are both on this box (oci
prefers podman), and the kernel (6.17, aarch64) supports Landlock at least through ABI
v2. `/dev/kvm` is absent, so `krun` stayed the placeholder the task asked for —
`tenon_sandbox::krun::probe()` always returns `Err("/dev/kvm absent")` here, or
`Err("krun backend arrives in P3.6")` on a machine that has `/dev/kvm` but no
implementation.

### Lines

| Part | LoC |
|---|---|
| `rs/sandbox/src/{lib,none,proc,oci,landlock,krun}.rs` | 192+45+44+221+107+9 = 618 |
| `rs/sandbox/tests/conformance.rs` | 164 |
| `rs/base/src/rpc.rs` (extracted from `base.rs` to stay under 600 lines) | 77 |
| `rs/base/src/{base,server,home,node,config,lib}.rs` (touched, not new) | 556+215+277+136+103+281 |
| `rs/cli/tests/gateway_gate.rs` | 276 |
| `beam/lib/tenon/beam/link/{handlers,server}.ex` (touched) | 80+166 |
| `sdk/py/tenon.py` (touched) | 308 |

New Rust: ~1135 lines (sandbox crate + its tests + the gate test + `rpc.rs`). Every
touched file is under the 600-line ceiling; `base.rs` was pulled back under it by moving
`Cmd`/`NodeView`/`Snapshot` into the new `rpc.rs` (a pure data/message module, no logic
moved).

### What landed

- **`Sandbox`/`Instance` traits reshaped.** `spawn` now returns `Arc<dyn Instance>`
  (`Box` would not let base clone a handle out to a `spawn_blocking` task while still
  holding one in `Node.sandbox`); `Instance` carries `id`, `backend`, `attach_addr`,
  `exec(cmd, args, timeout) -> ExecOutcome{status,stdout,stderr,timed_out}` and `destroy`.
  `detect()` returns the picked backend plus every skipped one's reason; `backend(name)`
  resolves `auto|oci|landlock|krun|none` and fails loud with the same reason for an
  explicit, unavailable choice.
- **oci** (`rs/sandbox/src/oci.rs`, 221 lines): podman-preferred/docker-fallback via a
  `PATH` scan, no client library — every operation shells out. `spawn` runs
  `<cli> run -d --name tenon-<env>-<nanos> --label tenon.env=<env> --memory <ram>m
  --pids-limit <n> -v <workspace>:/workspace [-v <gateway-dir>:<gateway-dir>:rw]
  [--network host] [-e TENON_GATEWAY=...] [-e NAME=value ...] <image> sleep infinity`.
  `exec` runs `<cli> exec <id> timeout -s KILL <secs> <cmd> <args..>` — the in-guest
  `timeout` (present in `python:3.12-alpine`, confirmed by hand before trusting it) is
  the actual enforcement; the outer `wait-timeout`-based runner in `proc.rs` is a
  backstop with a few extra seconds of grace, and a `137` exit status is also treated as
  `timed_out` in case the outer backstop never fires. `destroy` is `stop -t 2` then
  `rm -f`, idempotent behind an `AtomicBool`, and repeated by `Drop` as a safety net.
- **landlock** (`rs/sandbox/src/landlock.rs`, 107 lines): no persistent process — `spawn`
  just records the workspace and gateway-socket directory; `exec` restricts the *forked
  child* with `Command::pre_exec` (before `execve`, so the restriction covers the whole
  run) to read-only `/usr /lib /lib64 /bin /sbin /etc /proc/self` and read-write the
  workspace and the gateway directory, via `path_beneath_rules` + `restrict_self`. `probe`
  uses `CompatLevel::HardRequirement` against ABI v1 so an unsupported kernel fails loud
  instead of silently degrading; the actual per-exec ruleset uses best-effort compat
  (`ABI::V2`) so a slightly older-but-still-Landlock kernel still gets what it can enforce
  rather than erroring on every exec.
- **Shared exec runner** (`rs/sandbox/src/proc.rs`, 44 lines): spawns with piped
  stdout/stderr read on two threads, `wait_timeout` (crate `wait-timeout`) with a kill +
  `wait` on timeout. Used by both real backends so "kill on timeout" is one code path.
- **Base wiring**: `config.sandbox` defaults to `auto` (was `none`); `Home` grew
  `envs_dir/env_dir/workspace_dir/gateway_sock/gateway_address`; `node::spawn` now sets
  `TENON_HOME` (previously never passed to the node at all — its own `TENON_GATEWAY`
  default would have silently resolved against the *real* `$HOME` in every test, not the
  test's temp `TENON_HOME`, a P3.1a gap this closes) and, for agent-role nodes only,
  `TENON_GATEWAY` explicitly from `Home::gateway_address`, so base and the node agree on
  the exact same address without base having to parse the node's own default logic.
  `enter_sandbox` builds `Spec{workspace: home.workspace_dir(env), gateway:
  Some(home.gateway_address(env)), image: $TENON_SANDBOX_IMAGE, env_passthrough:
  $TENON_SANDBOX_ENV.split(',')}` and destroys+replaces the old instance on every
  `start()` (boot, restart-after-death, `reset`), matching the "destroyed at env stop and
  reset; reset re-creates" requirement without new state machinery — `start()` already ran
  on all three paths. `status`'s per-node `sandbox` field is now `{backend,id,attach}`
  instead of a bare id string.
- **Two new base RPCs**: `sandbox.exec{env,cmd,args,timeout}` and
  `sandbox.destroy{env}`, both documented as P3.1 test/CLI aids in `rs/README.md`. Both
  run the actual backend call via `tokio::task::spawn_blocking` so a slow container exec
  or `podman rm` never stalls the single-threaded base actor's handling of `status`/
  `health`/other envs' commands.
- **`svc{env,name,method,args}` proxy**: `server.rs` forwards it to the env's node as a
  raw `svc` frame (reusing the existing `Peer::request` path `health`/`tree`/`reload`
  already use, just with real params instead of `{}`). On the Elixir side,
  `Tenon.Beam.Link.Handlers.svc/2` (18 new lines) converts `name`/`method` from the wire's
  binaries to atoms exactly the way the kernel's own `wire_svc` handling already does
  (`to_atom/1` in `kernel/src/tenon.erl`, so the round trip through a wire-backed service
  lands on the same string the plugin declared), calls `:tenon.svc(root_ctx, name, method,
  args)`, and reports `{:error, reason}` results or a raised `{:error, _}`/`error(...)`
  the same way. `Link.Server` gained one new `incoming/2` clause (10 lines) to wire the
  frame in. Two new tests in `link_test.exs`: a successful proxy through a locally
  `provide`d service, and an unknown-service error. Beam suite: 20 tests (was 18), all
  passing; `mix credo --strict` clean, `mix format --check-formatted` clean,
  `mix compile --warnings-as-errors` clean.
- **`py/tenon.py` socket transport** (18 new lines): if `TENON_GATEWAY` is set,
  `Plugin()` connects a `socket.socket` (`unix:`/`tcp:` parsed the same way the gateway
  plugin itself parses its listen address) and wraps it with `sock.makefile(...)` for the
  read/write sides instead of opening fd 3/4; every frame past `hello` is unchanged, since
  the kernel's socket-backed fiber (P3.1a) is wire-protocol-identical to a port-backed
  one. Verified by hand with a throwaway unix-socket server script (hello -> load ->
  provide -> rep round-tripped correctly) before trusting it inside the real gate test.
  `sdk/test`'s existing 16-test fd-3/4 conformance suite (unaffected, still fd-based)
  stayed green.
- **Conformance suite** (`rs/sandbox/tests/conformance.rs`, 164 lines, one `check(name)`
  body parameterized over `"oci"`/`"landlock"`, `println!("skipping {name}: {reason}")`
  and an early return when `backend(name)` errors): spawn; `exec echo`; write a file
  inside (`/workspace/...` for oci, a bare relative path under `current_dir` for landlock)
  and read it back from the host; `sleep 5` against a 1 s timeout asserted `timed_out`;
  oci additionally reads `/sys/fs/cgroup/memory.max` from inside and asserts it equals the
  policy's `ram_mb * 1024 * 1024`; landlock additionally asserts a write to
  `/etc/tenon-landlock-should-fail` is denied (its equivalent of a resource-cap check,
  since Landlock has no memory concept); `destroy` then, for oci, `ps -a --filter
  label=tenon.env=<env>` is asserted empty.
- **The P3.1 gate** (`rs/cli/tests/gateway_gate.rs`, 276 lines): boots base with
  `sandbox: oci` in a throwaway `config.yml` (skips if neither `podman` nor `docker` is on
  `PATH`); copies `sdk/py/tenon.py` and a 6-line plugin script into the root env's
  workspace; `sandbox.exec`s `sh -c "nohup python3 /workspace/inside_plugin.py
  >/workspace/inside.log 2>&1 </dev/null & echo started"` (backgrounding inside the
  container so the exec itself returns immediately — confirmed by hand first that
  `podman stop` on the container's `sleep infinity` PID 1 takes the whole PID namespace,
  and everything in it, down within its grace period, since Linux kills every process in
  a PID namespace when its init dies); polls `status` until a new **non-failed** child
  appears under the root tree's `gateway` fiber (a socket close leaves a `failed` fiber in
  the tree rather than removing it — README section 11's wire v1.2 semantics — so
  "gone"/"appeared" both filter on `status != "failed"`, not on list length alone); calls
  `svc{env: root, name: inside, method: ping}` and asserts `"pong"`; calls
  `sandbox.destroy{env: root}` and polls until that fiber's status flips or it
  disappears, while confirming `status` itself keeps answering (base unaffected); runs
  `tenon reset --env root` as the "restart" alternative to a Link `unmount{id}` request
  (there is no such request yet — reset already tears down and remounts the whole node,
  gateway included, which is the RFC's other named option) and confirms `status` answers
  with the root node registered again afterward. Passed 3 consecutive runs, ~5.7 s each.

### Gates, full last lines

rs: `cargo build --release` — Finished, no warnings. `cargo clippy --all-targets -- -D
warnings` — Finished, no output. `cargo fmt --check` — exit 0. `TENON_RELEASE_DIR=...
cargo test` (`--test-threads=1`, run three times across the refactor) — stable every time:
1 (`base::token`) + 19 (adversarial) + 7 (`boot.rs`) + 1 (`gateway_gate.rs`) + 1
(`harness`) + 5 (`sandbox` unit) + 2 (`sandbox` conformance) + 3 (`storage`) + 1 (`worker`)
= 39 passed, 0 failed, every run.

beam: `mix compile --warnings-as-errors` clean. `mix format --check-formatted` clean (only
the pre-existing `link_test.exs` on `main`, noted in §18.5, was ever unformatted, and this
task's edits to that file are themselves formatted). `mix credo --strict` "142 mods/funs,
found no issues". `mix test` "20 tests, 0 failures". `MIX_ENV=prod mix release` assembled
and used by the rs gate above.

sdk: `cd sdk/test && mix test` — "16 tests, 0 failures" (unaffected fd-3/4 path).

### Deviations

1. **`krun` is exactly the placeholder the task asked for** — no libkrun dependency added,
   `probe()` always `Err`, reason names why (`/dev/kvm absent` here).
2. **A `kill -9` of base leaks that boot's oci container(s)** and `Sandbox::reap` exists
   but is deliberately not wired into the boot path — see `rs/README.md` deviation 11 for
   the full reasoning (wiring it made `sigterm_during_boot_leaves_no_zombies` flaky by
   blocking the actor on a `podman ps`/`rm -f` round trip during `Cmd::Boot`). Every
   container this work's own test runs created was confirmed removed by hand
   (`podman ps -a --filter label=tenon.env`) before finishing.
3. **`sandbox` defaulting to `auto` changes P3.0 test behavior**: the default profile's
   root env now runs inside a real `oci` container. `cli/tests/boot.rs`'s sandbox
   assertion was updated (it now accepts `oci` or `landlock`, whichever `auto` picks) and
   the adversarial suite got measurably slower (~95 s vs. the near-instant `none` path)
   but stayed green across three full runs. Memory/pids caps and egress allowlisting
   remain oci-only (RFC open question 4 is still open); landlock has no resource-cap
   concept to enforce them with.
4. **No `runtime.spawn`/child-env plumbing here** — one sandbox instance per *existing*
   env (root, plus whatever a future P3.2 `runtime.spawn` creates) is P3.1's scope; spawning
   child envs at all is P3.2.
5. **`sandbox.exec`/`sandbox.destroy` are base RPCs behind `tenon_base::client::Client`,
   not `tenon` CLI subcommands** — deliberately, per the task's "test aid" framing; adding
   CLI surface for something P3.2's worker tools supersede felt like scope creep.

## 21. P3.1c result: container hygiene fix (2026-08-18)

Closed the two gaps deviation 11 (`rs/README.md`) had left open. Containers now carry
`tenon.home=<sha256(home)[..12]>` and `tenon.base=<base pid>` alongside `tenon.env`, and
the container name embeds the home hash, so two homes both running a `root` env (every
test fixture does exactly that) can never be mistaken for each other's leftovers —
this is what made `sandbox/tests/conformance.rs`'s own leak assertion flaky under
parallel test runs before. `Sandbox::reap(home_hash, all)` now actually runs, once per
`base::foreground()` boot, on a `tokio::task::spawn_blocking` thread that reports back to
the actor as an ordinary `Cmd::SandboxReaped{count}` (a `sandbox.reaped` event) rather
than on the actor's own task, which is exactly what made an earlier synchronous attempt
break `sigterm_during_boot_leaves_no_zombies` under container backlog. A second bug
surfaced while auditing this: `Cmd::Stop`/`Cmd::AbortBoot` were replying "ok" to the
caller *before* `stop_nodes()` had actually destroyed each env's sandbox instance, so a
caller that trusted the reply and force-killed base a moment later (every adversarial
test fixture's `Drop` does exactly this) could interrupt an in-flight `podman
stop`/`rm -f` and orphan the container anyway — reordering both to reply only after
teardown completes fixed it. Humans get `tenon sandbox reap [--all]` and `tenon stop
--all` for the same operation outside a test. Gates: `cargo build --release`, `cargo
clippy --all-targets -- -D warnings`, `cargo fmt --check` all clean; `TENON_RELEASE_DIR=...
cargo test` green three consecutive full runs, 42/42 each time (two new: `base::home::hash`
unit test and `reap::a_leaked_container_with_a_dead_base_is_reaped_on_next_start`
adversarial test); `podman ps -a --filter label=tenon.home` showed 0 leftover containers
after every run, and the live user demo (`cli/tenon start
playground/web/tenon.yml`, a separate Elixir-side process that never touches `rs/`'s
sandbox at all) was left running throughout and never signaled.

### Next

P3.2: the worker as one resident process (in-process tools, pty ring buffers, step
git-snap, `.gitignore`, packs to host) registering via the gateway in sandboxed envs and
fd 3/4 otherwise; `runtime.spawn` so an agent node can create child envs as external
fibers.
