Read NOTES.md first: architecture, decision log, current phase design.

The kernel lives in `kernel/`. Read `kernel/README.md` before touching
`kernel/src/tenon.erl` — the code carries no comments, the README carries the
explanation.

Coding rules: ../vibe-forge/rules-template/elixir.md and ../vibe-forge/rules-template/universal.md.

All gates must pass before every commit:

```
cd kernel && mix compile && mix format --check-formatted && mix test
```
