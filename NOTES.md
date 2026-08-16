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

`Tenon.Bridge` plugin: `%{cmd, args, env, protocol, launcher}`.
protocol `:lines` (JSON per line) | `:mcp` | `:acp` | `:jsonrpc` (DSH sdk).
launcher `:port` (default) | `:vibe_term` (visible pane) | `:bwrap`/`:landlock` | `:microvm` | `:remote` (node/e2b).
Order: sh script -> MCP server -> DSH `--profile headless`/ACP whole agent.

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
4. Every registration returns a disposer. `Ctx.effect(ctx, fun)`: fun runs now, returns disposer or nil. Works from inside the fiber process (during apply; collected then merged into state) and from other processes (call to fiber). Disposers run reverse order in the fiber process on unload.
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

## 7. Open questions for reviewers

- Should `emit` really swallow hook errors (Cordis propagates)? We chose isolation for reliability.
- Process-dictionary accumulator for effects during apply: acceptable, or force `apply/2` to return disposers?
- Fiber process stays alive in `:failed`: fine, or let it die and keep a tombstone row?
