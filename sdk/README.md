# tenon SDKs — external plugins in python and typescript

Two tiny zero-dependency ports of the wire protocol in `../kernel/README.md`. Both speak
wire v1.1: 4-byte big-endian length + JSON, read from **fd 3**, written to **fd 4**.
stdin, stdout and stderr stay free for logs.

| | python | typescript |
|---|---|---|
| file | `py/tenon.py` (stdlib only, python 3.8+) | `ts/tenon.ts` (node >= 22, ESM, no deps) |
| example | `py/example.py` | `ts/example.ts` |
| run | `python3 py/example.py` (mounted by the kernel) | `pnpm exec tsc -p ts` then `node ts/dist/example.js` |

Mount either one from Elixir with
`:tenon.mount(ctx, %{cmd: ~c"python3", args: [~c"py/example.py"], config: %{...}})`.

## API

| python | typescript | meaning |
|---|---|---|
| `Plugin(inject=["db"])` | `new Plugin({inject: ["db"]})` | declares the injected services in `hello` |
| `@plugin.on(event, mode, prepend, arity)` | `plugin.on(event, handler, {mode, prepend, arity})` | register a hook; `mode` is `"emit"` or `"call"` |
| `plugin.off(handler)` | `plugin.off(hook)` | remove it |
| `plugin.provide(name, {method: fn})` | `plugin.provide(name, {method: fn})` | publish a service; methods take the call args positionally |
| `plugin.unprovide(name)` | `plugin.unprovide(name)` | withdraw it |
| `plugin.emit(event, args)` | `plugin.emit(event, args)` | fire and forget |
| `plugin.call(event, args)` | `await plugin.call(event, args)` | waterfall, returns the final args |
| `plugin.svc(name, method, args)` | `await plugin.svc(name, method, args)` | call another plugin's service |
| `@plugin.on_load` / `@plugin.on_unload` | `plugin.onLoad(fn)` / `plugin.onUnload(fn)` | lifecycle; `on_load` receives the config |
| `plugin.log(message)` | `plugin.log(message)` | one line to stderr |
| `plugin.run()` | `plugin.run()` | send `hello`, serve frames, exit 0 after `unload` |

Handlers: an `emit` hook is `(args) -> None`; a `call` hook is `(args, next) -> result`.
A service method is `(*args) -> result`. Anything a handler raises becomes a
`rep{error}` frame — the loop never dies on user code.

## The next/await rule

A `call`-mode hook either answers on its own or delegates. `next(args)` sends
`next{await: true}` and gives you the downstream result, so you can post-process it:

```python
@plugin.on("tools/execute", mode="call", prepend=True)
def guard(args, next):
    if "rm -rf" in args[0]["cmd"]:
        return {"status": "blocked"}
    return {"guarded": True, "result": next([dict(args[0], checked=True)])}
```

`args` handed to `next` must keep the arity of the original call. Not calling `next`
short-circuits the waterfall — downstream hooks and the terminal never run.

Both SDKs are re-entrant while awaiting: the python loop keeps serving inbound frames
inside `next`/`svc`/`call`, and the typescript one dispatches every frame as its own
task. A hook handler may therefore `svc` into another plugin — in either language, in
either direction — while the hook it is serving is still in flight.

## Bulk data by handle

The wire is a control plane. Never ship PTY bytes, DOM trees, file bodies or token
streams through it. Return a **handle** — a path, a unix socket, a URL, an fd, a stream
endpoint — and let the two plugins talk over that channel themselves. The kernel only
brokers discovery through services.

## Frame cap

The kernel spawns every plugin with `TENON_MAX_FRAME` (bytes) and
`TENON_KERNEL_DEADLINE` (ms) in its environment; both SDKs read them at startup into
`plugin.max_frame` / `plugin.maxFrame` and `plugin.deadline_ms` / `plugin.deadlineMs`.
A frame over the cap is never written: the SDK raises `FrameTooLarge`, which inside a
handler turns into `rep{error: "frame_too_large"}`, so the caller gets
`{:error, "frame_too_large"}` instead of a silent drop or a 30 s timeout.

## Conformance tests

`cd test && mix test` mounts both examples through the kernel and asserts they behave
identically — services, waterfall block and pass-through, emit counting, cross-language
nested calls, unmount ending the OS process, and the frame cap. The typescript example
is compiled in `setup_all`; `node_modules/` and `dist/` are not committed.
