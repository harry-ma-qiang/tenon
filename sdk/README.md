# tenon SDKs — external plugins in python, typescript and rust

Three tiny zero-dependency ports of the wire protocol in `../kernel/README.md`. All three
speak wire v1.1: 4-byte big-endian length + JSON, read from **fd 3**, written to **fd 4**.
stdin, stdout and stderr stay free for logs. `py/tenon.py` additionally speaks wire v1.2
over a socket when `TENON_GATEWAY` is set — see "TENON_GATEWAY socket mode" below, the
P3.1 path for a plugin started inside a sandbox to register with its node.

| | python | typescript | rust |
|---|---|---|---|
| file | `py/tenon.py` (stdlib only, python 3.8+) | `ts/tenon.ts` (node >= 22, ESM, no deps) | `rs/src/lib.rs` (crate `tenon-sdk`, only `serde_json`) |
| example | `py/example.py` | `ts/example.ts` | `rs/src/bin/example.rs` |
| run | `python3 py/example.py` | `pnpm exec tsc -p ts` then `node ts/dist/example.js` | `cargo build --release` then `rs/target/release/example` |

Mount any of them from Elixir with
`:tenon.mount(ctx, %{cmd: ~c"python3", args: [~c"py/example.py"], config: %{...}})`; the
rust example is its own executable, so `cmd` is the binary and `args` is `[]`.

## API

| python | typescript | rust | meaning |
|---|---|---|---|
| `Plugin(inject=["db"])` | `new Plugin({inject: ["db"]})` | `Plugin::new(&["db"])` | declares the injected services in `hello` |
| `@plugin.on(event, mode, prepend, arity)` | `plugin.on(event, handler, {mode, prepend, arity})` | `plugin.on(event, Mode::Call, prepend, arity, handler(f))` | register a hook |
| `plugin.off(handler)` | `plugin.off(hook)` | `plugin.off(hook)` | remove it |
| `plugin.provide(name, {method: fn})` | `plugin.provide(name, {method: fn})` | `plugin.provide(name, methods)` | publish a service; methods take the call args positionally |
| `plugin.unprovide(name)` | `plugin.unprovide(name)` | `plugin.unprovide(name)` | withdraw it |
| `plugin.emit(event, args)` | `plugin.emit(event, args)` | `plugin.emit(event, args)?` | fire and forget |
| `plugin.call(event, args)` | `await plugin.call(event, args)` | `plugin.call(event, args)?` | waterfall, returns the final args |
| `plugin.svc(name, method, args)` | `await plugin.svc(name, method, args)` | `plugin.svc(name, method, args)?` | call another plugin's service |
| `@plugin.on_load` / `@plugin.on_unload` | `plugin.onLoad(fn)` / `plugin.onUnload(fn)` | `plugin.on_load(f)` / `plugin.on_unload(f)` | lifecycle; the load handler receives the config |
| `plugin.log(message)` | `plugin.log(message)` | `plugin.log(message)` | one line to stderr |
| `plugin.run()` | `plugin.run()` | `plugin.run()` | send `hello`, serve frames, exit 0 after `unload` |

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

All three SDKs are re-entrant while awaiting: python and rust keep serving inbound frames
inside `next`/`svc`/`call`, and typescript dispatches every frame as its own task. A hook
handler may therefore `svc` into another plugin — in any language, in either direction —
while the hook it is serving is still in flight.

## Rust

```rust
use serde_json::{json, Value};
use tenon_sdk::{arg, handler, Mode, Next, Plugin};

fn main() {
    let mut plugin = Plugin::new(&[]);
    plugin.on_load(|config: Value, next: &mut Next| {
        let mut methods = std::collections::HashMap::new();
        methods.insert("ping", handler(|_args, _next| Ok(json!("pong"))));
        next.provide(config["service"].as_str().unwrap_or("demo"), methods);
        Ok(())
    });
    plugin.on(
        "tools/execute",
        Mode::Call,
        true,
        1,
        handler(|args: Vec<Value>, next: &mut Next| {
            if arg(&args, 0)["cmd"].as_str().unwrap_or("").contains("rm -rf") {
                return Ok(json!({"status": "blocked"}));
            }
            Ok(json!({"result": next.call(args)?}))
        }),
    );
    plugin.run()
}
```

One handler shape everywhere: `Fn(Vec<Value>, &mut Next) -> Result<Value>`, wrapped in an
`Rc` by `handler(..)`. Hooks, service methods and the two lifecycle callbacks all use it,
so a plugin is a set of closures over one `Rc<RefCell<State>>` — the rust stand-in for the
module-level `state` dict of `py/example.py`.

`Next` is the plugin itself, borrowed for the duration of one request. It is how a handler
talks back to the kernel while it is still serving:

| method | frame | meaning |
|---|---|---|
| `next.call(args)` | `next{await: true}` | continue the `call`-mode hook, return the downstream result |
| `next.svc(name, method, args)` | `svc` | call another plugin's service |
| `next.waterfall(event, args)` | `call` | run a waterfall on the bus |
| `next.emit(event, args)` | `emit` | fire and forget |
| `next.provide` / `unprovide` / `on` / `off` | registration | register from inside a handler, e.g. `on_load` |
| `next.log` / `config` / `max_frame` / `deadline_ms` | — | environment |

`next.call` outside a `call`-mode hook is an error, not a panic. The loop is a single
blocking thread over fd 3/4 with exact reads; a nested wait recurses into the same
`settle` loop, so re-entrancy costs stack, not threads, and no lock is ever held across a
wire round trip. Anything a handler returns as `Err` becomes `rep{error}`.

Both crates build with zero warnings under `cargo clippy --all-targets -- -D warnings`
and are `cargo fmt` clean.

## Bulk data by handle

The wire is a control plane. Never ship PTY bytes, DOM trees, file bodies or token
streams through it. Return a **handle** — a path, a unix socket, a URL, an fd, a stream
endpoint — and let the two plugins talk over that channel themselves. The kernel only
brokers discovery through services. `../plugins/term` is the worked example: output over
64 KiB becomes `{"handle": path, "bytes": n}` and the caller pages it back with
`term.read`.

## TENON_GATEWAY socket mode (python)

Wire v1.1 (fd 3/4) is how the host spawns a plugin directly. Inside a P3.1 sandbox
(`oci`, `landlock`) nothing spawns the plugin process with those descriptors wired up —
instead the plugin dials out to the gateway plugin listening in its node (RFC section 6,
kernel wire v1.2, `../beam/README.md`'s Gateway section). `py/tenon.py` picks the
transport automatically:

```python
import tenon

plugin = tenon.Plugin(inject=[])
plugin.provide("inside", {"ping": lambda: "pong"})
plugin.run()
```

If the `TENON_GATEWAY` environment variable is set (`unix:<path>` or `tcp:<host>:<port>`,
the same string the node itself was started with — see `../beam/README.md`), `Plugin()`
connects a `socket.socket` to that address instead of opening file descriptors 3 and 4,
and wraps it in a read side and a write side with `sock.makefile(...)`; everything past
that point — `hello`, `load`, `provide`, `svc`, `on`/hooks — is identical, since the
kernel treats a socket-backed fiber exactly like a port-backed one. Without
`TENON_GATEWAY` set, behavior is unchanged: fd 3/4, as before. Passing `wire_in`/`wire_out`
explicitly (tests, `example.py`) always wins over both.

Typescript and rust do not have this mode yet — only python plugins are expected to run
disposably inside a sandbox in P3.1; the other two SDKs stay fd 3/4-only until a runtime
needs them there too.

## Frame cap

The kernel spawns every plugin with `TENON_MAX_FRAME` (bytes) and
`TENON_KERNEL_DEADLINE` (ms) in its environment; the SDKs read them at startup into
`plugin.max_frame` / `plugin.maxFrame` / `plugin.max_frame()` and the matching deadline.
A frame over the cap is never written: the SDK raises `FrameTooLarge` (rust:
`Error::FrameTooLarge`), which inside a handler turns into `rep{error: "frame_too_large"}`,
so the caller gets `{:error, :frame_too_large}` instead of a silent drop or a 30 s timeout.

## Conformance tests

`cd test && mix test` mounts all three examples through the kernel and asserts they behave
identically — services, waterfall block and pass-through, emit counting, cross-language
nested calls (py<->ts, rs<->py), unmount ending the OS process, and the frame cap — plus
the `plugins/term` cases (exec, spill to a handle, spawn/kill, `term/exit`, unmount killing
children). 16 tests. The typescript example is compiled and both rust crates are built in
`setup_all`; `node_modules/`, `dist/` and `target/` are not committed.
