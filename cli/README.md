# tenon — the command line entry point

An escript (app `:tenon_cli`, 295 lines in `lib/`) that wraps the atom kernel
(`../kernel`) and the config loader (`../loader`): it composes Cordis/DSH config layers
and either mounts them or just reports what they resolve to.

```
cd cli && mix escript.build      # produces ./tenon
```

## Commands

| Command | What it does |
|---|---|
| `tenon start <layer.yml>...` | start a kernel, mount `Tenon.Loader` with those layers, print the tree, stay alive |
| `tenon dump <layer.yml>...` | compose the layers and print every row with its resolved kind. Mounts nothing |
| `tenon check <layer.yml>...` | compose only; print unknown names, bad rows and patch warnings. Exit 1 if any row failed |

Options (all three commands): `--registry MOD_OR_FILE`, `--dsh-home DIR`,
`--dsh-root DIR`, `--dsh-bridge FILE`, `--profile NAME`. A layer ending in `.patch.yml`
is a patch list, any other layer is an entry list; layers apply in the order given.

```
$ ./tenon dump demo/cordis.yml --registry demo/registry.yml
id     kind      name           parent  detail
calc   external  py:calculator  -       -
grp    group     cordis:group   -       -
guard  external  py:guard       grp     -

$ ./tenon check demo/cordis.yml
error: row calc (py:calculator) {:unknown_name, "py:calculator"}
3 rows, 3 errors, 0 warnings          # exit 1

$ ./tenon start demo/cordis.yml --registry demo/registry.yml
id     kind      name           parent  detail
calc   external  py:calculator  -       active
...
tenon: os pid 3792326, SIGHUP reloads, SIGTERM stops, Ctrl-C aborts
```

## Registry

`cordis:group` and the DSH collapse (`--dsh-home`) are built in; every other `name` in a
layer must come from `--registry`, otherwise the row fails with `{:unknown_name, name}`.
The source is a `.yml` map, a `.exs` file evaluating to a map, or a module exporting
`registry/0`.

```yaml
"py:calculator":
  cmd: /usr/bin/python3
  args: ["../playground/plugins/math_calculator.py"]
  env: {TENON_DEMO: "1"}
"tenon:policy":
  module: My.Policy          # only resolvable if the module is in the escript
```

## DSH

`--dsh-home DIR --dsh-root DIR --dsh-bridge ../bridge/dsh/dist/plugin.js` enables the
built-in collapse target: every `@deepseek-ai/dsh-*` row (or `tenon: dsh` row) leaves the
Tenon tree, is written into `$DSH_HOME/profiles/<profile>/cordis.patch.yml`, and DSH is
mounted as one external fiber. `dump` and `check` only *report* the collapse; the profile
files are written by `start`. See `../loader/README.md` and `../bridge/dsh/README.md`.

## Signals

`SIGHUP` calls `Tenon.Loader.reload/1` — layers are re-read and the diff is applied in
place. `SIGTERM` / `SIGQUIT` unmount the loader and stop the kernel, so external plugins
get their `unload` frame and the grace period. `SIGINT` (Ctrl-C) is not routable by
`os:set_signal/2`; the emulator break handler aborts the VM at once and plugins exit on
wire EOF instead.

## Tests

```
mix compile --warnings-as-errors && mix format --check-formatted
mix credo --strict && mix test
```

6 tests. They run `dump` and `check` in process against the `../loader/test/fixtures`
trees (tree, DSH composition + patch layer) and assert the resolved kinds, the collapse
line, the exit codes and that nothing is mounted or written.
