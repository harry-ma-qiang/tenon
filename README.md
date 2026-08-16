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

## Layout

| Path | What | Language |
|---|---|---|
| [`kernel/`](kernel/README.md) | the atom kernel, `src/tenon.erl`, zero deps | Erlang |
| [`loader/`](loader/README.md) | config tree plugin: yml layers, patches, groups, DSH collapse | Elixir |
| [`cli/`](cli/README.md) | the `tenon` escript: `start` / `dump` / `check` | Elixir |
| [`sdk/`](sdk/README.md) | wire SDKs `py/`, `ts/`, `rs/` + `test/` conformance suite | Python, TS, Rust |
| [`bridge/dsh/`](bridge/dsh/README.md) | `tenon-bridge`: the whole DeepSeek Harness as one plugin | TypeScript |
| [`plugins/term/`](plugins/term/README.md) | `tenon-term`: process runner, the worked handle example | Rust |
| `playground/` | scratch plugins and a DSH home, gitignored | — |

## Quick start

Toolchain: OTP 27 and Elixir 1.18 (`mise use -g erlang@27 elixir@1.18-otp-27`), plus
node 22+ with pnpm for the TypeScript parts, a stable rust toolchain for `sdk/rs` and
`plugins/term`, and python3 for `sdk/py`. Only the first pair is needed for the kernel.

```
cd cli && mix escript.build          # -> cli/tenon
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
| `sdk/rs/`, `plugins/term/` | `cargo build --release`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` |
| `sdk/ts/`, `bridge/dsh/` | `pnpm exec tsc --noEmit` |

Coding rules for agents and humans: `AGENTS.md`.

## License

MIT, see [LICENSE](LICENSE). Tenon is a functional port of the Cordis kernel concepts; no
Cordis source is copied or redistributed here, see [NOTICE](NOTICE).
