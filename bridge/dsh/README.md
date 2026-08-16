# bridge/dsh — `tenon-bridge`

The whole DeepSeek Harness (DSH) runs unmodified, in **one** Node process, as **one**
external plugin of the Tenon atom kernel. `tenon-bridge` is the Cordis plugin that lives
inside that process: it opens the Tenon wire on fd 3/4 and mirrors a declared set of DSH
events and services onto the Tenon bus.

Compatibility level **L2 + L3** from `../../NOTES.md` §12: DSH plugins keep running on
real Cordis, and a manifest projects selected seams onto Tenon. Per-plugin Tenon fibers
for TS plugins remain out of scope.

```
Tenon kernel (BEAM)                        one Node process
  root fiber
   |- fiber guard.py  <---- call ----.
   |- fiber "dsh" (Port, fd 3/4) <====> tenon-bridge -> ctx.on('tools/pre-execute')
        service :dsh                     (Cordis plugin)  ctx.tools / sessions / agents
```

## How it boots

`tenon-bridge` is an ordinary out-of-tree Cordis row. `apply(ctx, config)`:

1. `fstat`s fd 3 and fd 4. If either is missing (DSH started standalone) it writes one
   line to stderr and stays inert — the profile still boots.
2. Constructs the `sdk/ts` `Plugin` with `hello{inject: []}` and calls `run()`.
3. On the Tenon `load` frame it awaits `ctx.get('loader').await()` (DSH assembles its
   tree concurrently; services only exist afterwards), registers the mirrors, `provide`s
   the `dsh` service, and answers the frame. The fiber is `active` from here.
4. On `unload` it disposes every mirror and registration and exits with 0: "the fiber is
   loaded" and "the OS process runs" are the same fact.

## Profile recipe

`profile/` is the reference copy of a working profile (the live one the tests use is
`../../playground/dsh-home/profiles/tenon/`): `package.json` selects the bundles,
`cordis.patch.yml` inserts the bridge row. Paths resolve **relative to the profile dir**.

```yaml
- insert:
    - id: tenon-bridge
      name: ../../../../bridge/dsh/dist/plugin.js
      config:
        demoTool: tenon_echo
        services:
          - name: dsh
        events:
          - name: tools/pre-execute
            mode: call
            pick: [name, arguments, callId]
```

## Manifest

`config.events[]` — one mirror per row: `name` (used unchanged on both buses), `mode`
(`emit` or `call`), `pick` (allowlist of object keys), `prepend`, and `deny` — the value
handed back to DSH when Tenon rejects, with `$reason` substituted (default
`{kind: deny, reason: $reason}`).

`config.services[]` — `{name, methods?}`. `methods` may be omitted (expose everything),
an array (allowlist), or a map of `exposedName: internalName`. Methods today: `ping`,
`pid`, `mirrors`, `tools.list`, `tools.execute`, `sessions.list`, `sessions.create`,
`agents.list`.

`config.demoTool` — register a `tenon_echo` tool (`{text}` in, `{text}` out) so the
pipeline can be exercised without a model. Omit it in production profiles.

## Projection and pick

The wire is JSON and capped (`TENON_MAX_FRAME`); DSH arguments are live class instances
with getters, `AbortSignal`s, cycles and functions. They are **projected**: functions,
symbols and `undefined` are dropped, `bigint` and non-finite numbers become strings,
strings are cut at 4096 chars, depth is capped at 4 (`"[depth]"`), arrays and key counts
at 64, cycles become `"[circular]"`, and a throwing getter is skipped. Over 75% of the
frame cap every argument becomes `{truncated: true, bytes: N}`, preserving the arity.
With `pick`, keys are read directly off the object so getters work; without it only own
enumerable keys are visible, usually empty for a DSH class instance — **always set
`pick`.**

`call` mode: the DSH listener `(...args, next)` sends `plugin.call(name, projection)`. A
plugin-originated `call` runs against an identity terminal, so an array of the same length
means "allowed": the `pick` keys from it are written **back onto the original DSH
objects** (never a replacement object; a frozen field is logged and skipped) and then
`next()` runs. Anything else — `{deny: reason}` or any other short-circuit value — is
rendered through the `deny` template and returned instead of calling `next()`.

## Mirrored today

| Event | Mode | Pick |
|---|---|---|
| `session/created` | emit | `[id]` |
| `session/event` | emit | `[id, type]` |
| `tools/pre-execute` | call | `[name, arguments, callId]` — a Tenon hook can deny a tool call |

## Running it

```
pnpm install && pnpm run build          # dist/tenon.js (the sdk) + dist/plugin.js
DSH_HOME=../../playground/dsh-home node <dsh>/apps/cli/lib/bin.js --profile tenon
DSH_HOME=... node --import tsx/esm <dsh>/apps/cli/src/bin.ts --profile tenon   # source
cd test && mix test
```

The built launcher needs `build:lib:host` **and** `build:lib:client` in the DSH repo —
three packages the profile loads (`typert-registry`, `api-gateway`, `client-connection`)
are client-face bundles. The source launcher needs only the host build but must run with
the DSH repo as cwd so `tsx/esm` resolves. `test/bridge_test.exs` mounts `test/guard.py`
(a `sdk/py` plugin whose `tools/pre-execute` hook denies `rm -rf`) and then DSH, checks
the `dsh` service and the mirror list, runs `tenon_echo` twice (allowed, then denied by
the Tenon-side guard), checks that a `session/created` emit reaches the guard, and
unmounts, asserting the Node process is gone.

## Deviations

1. **`inject` is not used.** Cordis would reload the plugin when a provider changes, which
   would re-open the wire. The bridge waits on `loader.await()` instead.
2. **`tools/pre-execute` cannot rewrite arguments.** DSH calls `next()` with no arguments
   and documents input rewriting as deliberately excluded, so the merge-back is
   implemented but has nothing to write on this event.
3. **Denial shape is config, not code.** The bridge imports no DSH types; the `deny`
   template in the manifest is what DSH receives (`{kind: 'deny', reason}` here).
4. **Programmatic execution is unscoped.** `tools.execute` calls
   `ctx.tools.execute({callId, name, arguments, signal})` with no agent, so scope-filtered
   listeners do not see it. Enough for the guard loop; no model turn is needed.
5. **Fail-loud.** DSH's `installFailLoud` exits on any unhandled rejection, so every async
   path here is wrapped and every failure becomes a `rep{error}` frame.
6. **The headless runner is not used**, and the wire check is best effort: fd 3/4 can
   exist but be unwritable, in which case the first `hello` write fails, is logged, and
   the plugin stays inert.
