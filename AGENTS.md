Read `NOTES.md` first: architecture, decision log, phase results. Sections 9-14 are the
current design (atom kernel, wire, loader, DSH bridge); section 15 is the status snapshot.
Before changing any subproject, read its own README — the code carries no comments, the
README carries the explanation.

## Subprojects and gates

Run the gates of every project you touched, in that project's directory, before every
commit. Report the full output; never claim a clean run after filtering it.

| Project | Read first | Gates |
|---|---|---|
| `kernel/` | `kernel/README.md`, NOTES 9-11 | `mix compile`, `mix format --check-formatted`, `mix test` |
| `loader/` | `loader/README.md`, NOTES 12-13 | the same plus `mix credo --strict` |
| `cli/` | `cli/README.md` | the same plus `mix credo --strict`, `mix escript.build` |
| `sdk/py`, `sdk/ts`, `sdk/rs` | `sdk/README.md` | `cd sdk/test && mix test` (mounts all three) |
| `sdk/rs`, `plugins/term` | `plugins/term/README.md` | `cargo build --release`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` |
| `sdk/ts`, `bridge/dsh` | `bridge/dsh/README.md`, NOTES 14 | `pnpm exec tsc --noEmit` (pnpm only, never npm) |
| `bridge/dsh/test` | `bridge/dsh/README.md` | `mix test` (needs a built DSH and a built bridge) |

## Rules

* `../vibe-forge/rules-template/universal.md` — 600 lines per file, no comments, no emoji,
  read before writing, stay in scope, honest reporting, no secrets.
* `../vibe-forge/rules-template/elixir.md` — zero warnings, credo strict, `mix format`,
  `@spec` on public functions, behaviour before implementations, let it crash for bugs.
* `../vibe-forge/rules-template/tools.md` — the Rust section (`cargo clippy` clean on
  files you touched) and the TypeScript section (pnpm only).

`mix.lock` and `Cargo.lock` are committed: Elixir convention, and every rust crate here is
a binary. `_build/`, `deps/`, `target/`, `node_modules/`, `dist/` and `playground/` are not.

## Working notes

Append results to `NOTES.md` rather than adding new documents. Decisions go in the decision
log (section 0); a finished phase gets its own numbered section with what was built, what
was measured, and what was deliberately left out.
