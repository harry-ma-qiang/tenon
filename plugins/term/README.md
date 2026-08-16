# tenon-term — the first native infrastructure plugin

A rust external plugin (crate `tenon-term`, built on `../../sdk/rs`) that runs OS
processes for the rest of the bus. It is the minimum useful data-plane citizen: the kernel
brokers discovery, the plugin owns the processes, and bulk output leaves by handle.

```
:tenon.mount(ctx, %{cmd: ~c"plugins/term/target/release/tenon-term",
                    config: %{"service" => "term"}})
```

`config.service` renames the published service (default `term`).

## Service `term`

| method | args | result |
|---|---|---|
| `exec` | `cmd, args?, cwd?, timeout_ms?` | `{status, stdout, stderr, truncated}` |
| `spawn` | `cmd, args?` | `{pid, log}` |
| `kill` | `pid` | `{pid, status}` |
| `read` | `handle, offset?, len?` | the bytes as a string |
| `ping` / `pid` | — | `"pong"` / the plugin's own OS pid |

* `status` is the exit code, `128 + signal` for a signalled child, and `-1` for a child
  the plugin killed because it outran `timeout_ms`. An empty `cwd` means "inherit".
* `stdout` and `stderr` are inlined as strings only while they are valid UTF-8 and fit the
  inline cap (64 KiB, further clamped to `TENON_MAX_FRAME / 8` so a reply can never be
  refused by the frame cap). Anything larger is written to a temp file and returned as
  `{"handle": path, "bytes": n}`; `truncated` is true when either stream spilled. This is
  the bulk-data-by-handle rule of `../../kernel/README.md`, not a convenience.
* `read` pages a handle back: `len` defaults to, and is clamped by, the same inline cap, so
  a caller walks a large log with repeated offsets instead of one oversized frame.
* `spawn` is detached in the sense that matters here — stdin is `/dev/null`, stdout and
  stderr go to the returned log path, and nothing on the wire waits for it. The plugin
  keeps the child handle so it can reap and kill it; there is no `setsid`.
* `kill` only accepts pids this plugin spawned. Anything else is `{:error, "unknown pid N"}`
  — the plugin is not a general-purpose `kill(1)` for the bus.

## Event `term/exit`

`{pid, status}` is emitted when a spawned process ends. The wire loop is single threaded
by design, so the plugin notices exits at the top of every request it serves (`reap`) and
announces them from the loop itself — no second thread ever writes to fd 4. `kill` waits
for its own child and emits before it replies, so that case is immediate; a natural exit
surfaces on the next call, and `term.ping` is the cheap way to poke it.

## Lifetime

Every handle lives in `$TMPDIR/tenon-term-<pid>/`. On `unload` the plugin kills every
process it spawned, waits for them, removes that directory and exits 0 — unmounting the
fiber leaves nothing behind. Killing children on unload is deliberate: a plugin's process
lifetime equals its loaded state (`../../kernel/README.md`), and orphaned `sleep 30`s that
outlive their supervisor are worse than a lost background job.

## No PTY

There is no pty, no resize, no scrollback, no session multiplexing. `exec` and `spawn`
here are plain pipes. The pty multiplexer is `vibe-term`'s job (the future `term-core`,
see `../../../note.md` section B): when it lands it mounts as its own plugin and returns
pty handles over this same wire. This crate exists so the bus has a native process runner
today, and so the handle rule has a worked example.

## Build and test

```
cargo build --release
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cd ../../sdk/test && mix test        # exec, spill + read, spawn/kill, term/exit, unmount
```
