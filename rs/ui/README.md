# rs/ui — the barebone's own face

RFC-P3-minimal-harness.md section 6b: one dependency-free ASCII renderer, two carriers. This
crate (`tenon-ui`) is standalone (P3.5): no wiring into `base` or `harness` yet, no sqlite, no
RPC client. It only knows `UiModel`, a plain struct the caller fills from `status`,
`events.tail`, `session.history` and `approval.request` JSON.

## Model

`UiModel` (`src/model.rs`): `envs: Vec<NodeInfo>` (name, role, status, sandbox, restarts,
nested `children`), `transcript: Vec<TranscriptItem>` (role user/assistant/tool, text, and for
tool items a `tool_name` + `line_count`), `expanded: HashSet<usize>` (transcript indices shown
unfolded), `events: Vec<EventLine>` (ts, kind, one-line summary — kinds match the harness's
`session/created`, `user/message`, `turn/start`, `step/start`, `tool/call`, `tool/result`,
`assistant/message`, `turn/end`), `approvals: Vec<Approval>` (id, env, reason), `status:
StatusLine` (base pid, attached count, an optional pre-formatted budgets line), and
`input_hint: String`.

## Layout rules

`render(model, cols, rows) -> String` is pure: same input, same output, no I/O, no ANSI, no
tabs, borders drawn with `+ - |` only. It always returns exactly `rows` lines (`\n`-joined, no
trailing newline) and no line exceeds `cols` characters.

Responsive by `cols`:
- `< 80`: one column, stacked — tree, transcript, events, approvals, then a plain status line
  and a plain input-hint line (no border) at the very bottom.
- `80..=140`: two columns — tree on the left, transcript over approvals on the right — with an
  events box spanning the full width underneath, then status/input.
- `> 140`: three columns — tree | transcript | events, each full height — with an approvals box
  and status/input spanning the full width underneath.

Vertical space is split with `boxes::split_rows`, an integer proportional splitter that always
sums exactly to the rows available (no rounding drift), so the layout degrades gracefully at
small sizes instead of panicking or overflowing. Tool transcript items render as `[+] tool
<name> (<n> lines)` when their index is not in `model.expanded`, and `[-] tool <name> (<n>
lines)` followed by the tool text when it is. Long lines are wrapped at the box's inner width by
`wrap::wrap_line`; tabs are always expanded to spaces before wrapping.

## Carriers

- `html(model, cols) -> String` (`src/html.rs`): the same rendering wrapped in a minimal HTML
  page — a `<pre>` block (HTML-escaped), a `<form method=post action=/prompt>` textarea, one
  `<form method=post action=/approve/<id>>` per pending approval with approve/deny submit
  buttons, and a `<form method=post action=/rollback>`. The page works with no JavaScript; a
  small inline script (viewport width / character width) reloads with `?cols=N` only as an
  enhancement. This is what `tenon serve --http` (base, feature-gated, not yet wired) will
  return from `GET /`.
- `terminal::Frame` (`src/terminal.rs`): `Frame::size()` reads the terminal size via a
  `TIOCGWINSZ` ioctl (falls back to 80x24 off a real tty); `Frame::draw(model)` and
  `Frame::draw_at(model, cols, rows)` prepend an ANSI clear+home (`\x1b[2J\x1b[H`) to the
  rendered text. This is what `tenon attach --ui` will call each redraw.
- `keys::parse(byte) -> Key` (`src/keys.rs`): maps `p`/`a`/`r`/`q` (case-insensitive) to
  `Prompt`/`Approve`/`Rollback`/`Quit`, digits `0`-`9` to `Fold(n)`, everything else to `Other`.
  No raw-mode terminal handling here — the CLI attach loop owns that.

## Wiring later (P3.5 base/CLI work, out of scope for this crate)

`tenon attach --ui` will poll `status` + `events.tail` through the existing UDS client, build a
`UiModel` from the JSON, call `Frame::draw`, and feed raw stdin bytes through `keys::parse`.
`tenon serve --http` will do the same per request and call `html`. Both carriers are read-mostly
except for the `p`/`a`/`r` actions, which map to `session.prompt`, `approval.request` (answer),
and `reset`/rollback RPCs already on the wire.

## Tests

`cargo test -p tenon-ui` runs: unit tests in every `src/*.rs` module (box drawing, wrapping,
folding, escaping, key parsing, terminal framing); `tests/golden.rs`, three snapshot comparisons
at 60x20, 100x30 and 160x40 against files in `tests/golden/`; `tests/property.rs`, a hand-rolled
LCG that samples ~500 `(cols, rows)` pairs in `40..200 x 10..60` and asserts the no-overflow /
exact-row-count invariants hold, plus an empty-model no-panic pass; `tests/fold.rs`, that
expanding a transcript index reveals its tool text without breaking the row/column bounds.

To regenerate the golden files after a deliberate layout change:

```
TENON_UI_BLESS=1 cargo test -p tenon-ui --test golden
```

then diff `rs/ui/tests/golden/*.txt` before committing — a golden diff is the layout change,
review it like one.
