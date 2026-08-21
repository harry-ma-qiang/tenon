# Tenon

Tenon is a microkernel for composing long-running agent systems on the BEAM. The kernel is
one Erlang module: a registry of plugins, services and hooks with lifecycle, dependency
gating, event dispatch and effect disposal, plus a wire protocol so a plugin can be a
process in any language. Everything else — the config loader, the YAML dialect, the DSH
bridge, the language SDKs, the CLI — is a plugin or a caller outside the kernel.

A running system is a tree of **fibers**. Each fiber owns one **plugin** instance. Plugins
publish **services** and register **hooks**; out-of-VM plugins speak the **wire** (4-byte
length + JSON **frames** on fd 3/4) and pass bulk data by **handle**, never as frames. When
a fiber dies, cleanly or violently, nothing it registered survives it.

Design reference: Cordis (MIT), see NOTICE. Decisions, phase plans and status: `NOTES.md`.

Status: P4 complete (P4.0-4.8; P4.3 warm segments deliberately deferred to P5), independently QA'd
2026-08-20: all gates green (beam 51, Rust unit 101 + the P4 integration gates), end-to-end verified
with a real model on the single-file release binary (bus/kv/timer, `tenon run`, ingress, webhook, MCP
both directions, backup/restore replay, `tenon doctor`), the five P4.4 env-isolation defects found
and fixed, secret scan clean. Performance well inside every RFC budget (bus fan-out 282k msg/s,
publish->subscriber p99 1.3 µs; query text p99 0.48 ms and scan p99 70 ms at 1M events). Evidence and
how-to-review: `REVIEW-P4.md`. P3's earlier pass: `REVIEW-P3.md`. Next: P5 memory + navigator.

## Layout

| Path | What | Language |
|---|---|---|
| [`kernel/`](kernel/README.md) | the atom kernel, `src/tenon.erl`, zero deps | Erlang |
| [`loader/`](loader/README.md) | config tree plugin: yml layers, patches, groups, DSH collapse | Elixir |
| [`cli/`](cli/README.md) | the `tenon` escript: `start` / `dump` / `check` | Elixir |
| [`beam/`](beam/README.md) | `tenon_beam`: the node release, link + guardian plugins | Elixir |
| [`rs/`](rs/README.md) | the `tenon` binary and the barebone: base, storage, sandbox, roles | Rust |
| [`sdk/`](sdk/README.md) | wire SDKs `py/`, `ts/`, `rs/` + `test/` conformance suite | Python, TS, Rust |
| [`bridge/dsh/`](bridge/dsh/README.md) | `tenon-bridge`: the whole DeepSeek Harness as one plugin | TypeScript |
| [`plugins/term/`](plugins/term/README.md) | `tenon-term`: process runner, the worked handle example | Rust |
| `playground/` | scratch plugins and a DSH home, gitignored | — |

## Quick start

One file. `tenon` is a single binary with the whole BEAM release — ERTS, kernel, loader,
guardian and gateway — embedded as a payload and unpacked into `~/.tenon/erts/` the first
time it runs. Nothing else has to be installed to *use* it: no Erlang, no Elixir, no
container engine (a sandbox backend is detected, and the barebone boots without one).

```
curl -fsSLO https://github.com/<owner>/tenon/releases/latest/download/tenon-linux-x86_64
sha256sum -c tenon-linux-x86_64.sha256 && chmod +x tenon-linux-x86_64 && mv tenon-linux-x86_64 ~/bin/tenon

tenon start                       # base + guardian node G + the root environment
tenon status                      # what is up, which sandbox backend, which release
tenon run "summarise this repo"   # one task, streamed out of the session log
tenon attach --ui                 # the built-in ASCII UI
tenon stop
```

Point it at a model first — `~/.tenon/profiles/root/harness.yml` holds the provider, the
model and the *name* of the environment variable your key lives in; the key never enters
the sandbox. `rs/README.md` is the reference for the home layout, the roles and the
sandbox backends, `deploy/README.md` for running it under systemd or launchd.

To build that binary yourself, or to work on Tenon:

```
scripts/build-release.sh --verify     # -> dist/tenon-<os>-<arch> + .sha256
```

Toolchain for that, and for everything below: OTP 27 and Elixir 1.18
(`mise use -g erlang@27 elixir@1.18-otp-27`) plus a stable rust toolchain; node 22+ with
pnpm for the TypeScript parts and python3 for `sdk/py`. Only the first pair is needed for
the kernel alone.

```
cd cli && mix escript.build          # -> cli/tenon, the config-tree CLI below
```

Run a python plugin from a config tree. Two files, both relative to `cli/`:

```yaml
# demo/cordis.yml
- id: calc
  name: py:calculator

# demo/registry.yml
"py:calculator":
  cmd: /usr/bin/python3
  args: ["../playground/plugins/math_calculator.py"]
```

```
./tenon check demo/cordis.yml --registry demo/registry.yml   # exit 1 on any bad row
./tenon dump  demo/cordis.yml --registry demo/registry.yml   # rows and resolved kinds
./tenon start demo/cordis.yml --registry demo/registry.yml   # mount and stay alive
```

`start` prints the tree and keeps running: SIGHUP re-reads the layers and applies the diff,
SIGTERM unmounts and stops. Run DSH as one plugin by adding `@deepseek-ai/dsh-*` rows and
pointing the collapse at a DSH checkout:

```
./tenon start demo/cordis.yml --dsh-home ~/.dsh --dsh-root ../../deepseek-harness \
              --dsh-bridge ../bridge/dsh/dist/plugin.js
```

Prerequisites for that line: a DSH checkout with `pnpm install`, `pnpm run build:lib:host`
and `build:lib:client`, and `cd bridge/dsh && pnpm install && pnpm run build`.

## DSH compatibility

* **L1 — config files.** `cordis.yml`, `cordis.patch.yml`, profiles and bundles are
  accepted unchanged and compose into the same tree (`loader/`).
* **L2 — TS plugins.** DSH plugins run unmodified on real Cordis inside one Node process,
  mounted as one Tenon fiber (`bridge/dsh/`).
* **L3 — shared bus.** Selected DSH services and events are mirrored onto the Tenon bus by
  manifest, so a plugin in any language can deny a DSH tool call.

## Gates

Every commit, in each project you touched:

| Project | Gates |
|---|---|
| `kernel/` | `mix compile` (erlc warnings as errors), `mix format --check-formatted`, `mix test` |
| `loader/`, `cli/` | the same plus `mix credo --strict` |
| `sdk/test/`, `bridge/dsh/test/` | `mix test` |
| `beam/` | the loader gates plus `MIX_ENV=prod mix release` |
| `rs/` | `cargo build --release`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`, `cargo test --all-features` (with `TENON_RELEASE_DIR` set; the adversarial suite on its own) |
| `sdk/rs/`, `plugins/term/` | `cargo build --release`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` |
| `sdk/ts/`, `bridge/dsh/` | `pnpm exec tsc --noEmit` |

The same gates run on every push and pull request (`.github/workflows/ci.yml`); a `v*` tag
builds and publishes the single binary for linux-x86_64, linux-aarch64 and macos-arm64
(`.github/workflows/release.yml`).

Coding rules for agents and humans: `AGENTS.md`.

## AI self-evolution

Tenon is small on purpose: a kernel a machine can read, understand, and improve.
To test that, an off-the-shelf CLI coding agent was let loose — sandboxed,
throttled, and fully audited — to read the kernel from source and propose
improvements without ever touching the real tree. Reading only the source, it
found a real, unpatched denial-of-service in the kernel (atom-table exhaustion
via wire-supplied strings) and a genuine spec-versus-implementation drift in the
message bus; both were verified by hand. The full write-up, method, verdict, and
reproducibility notes: [`docs/ai-self-evolution.md`](docs/ai-self-evolution.md).

## License

MIT, see [LICENSE](LICENSE). Tenon is a functional port of the Cordis kernel concepts; no
Cordis source is copied or redistributed here, see [NOTICE](NOTICE).
