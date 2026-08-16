# tenon_loader — config tree plugin

An in-VM Tenon plugin that reads Cordis/DSH config files, composes one entry tree, and
mounts it on the atom kernel as fibers (P2.2 of `../NOTES.md` section 12). The loader is a
plugin like any other, so unmounting it unwinds everything it mounted.

```elixir
{:ok, kernel} = :tenon.start_link()
ctx = :tenon.root(kernel)

{:ok, loader} =
  :tenon.mount(ctx, %{
    module: Tenon.Loader,
    id: "loader",
    config: %{
      layers: ["cordis.yml", "profile/cordis.patch.yml"],
      registry: %{
        "tenon:policy" => %{module: My.Policy},
        "tenon:shell" => %{cmd: "/usr/bin/python3", args: ["plugin.py"], env: []}
      },
      collapse: [{"@deepseek-ai/dsh-", &My.Bridge.spec/1}]
    }
  })
```

## Config

| Key | Meaning |
|---|---|
| `layers` | ordered list of config layers. A path ending in `.patch.yml` / `.patch.yaml` is a **patch list**, any other path is an **entry list**. `{:patch, path_or_rows}` and `{:entries, path_or_rows}` state the kind explicitly and accept already-parsed rows |
| `registry` | `name => spec`. `%{module: M}` mounts an in-VM plugin, `%{cmd: c, args: a, env: e}` mounts an external one. A name that is in no layer of the registry **fails loud** |
| `collapse` | `[{prefix, fun}]`. Every row whose `name` starts with `prefix` leaves the tree; the harvested rows are handed to `fun` as ONE list and the returned spec is mounted as a single external fiber (the DSH bridge of P2.3). First matching prefix wins |

Every layer is turned into patch rows and applied over an **empty root** in order, exactly
like `composeEntries` in `packages/boot/app-boot/src/profile.ts`: an entry list becomes one
`insert` patch, a patch list contributes its rows as-is. Bundle layers, the profile patch,
the home patch and `--patch` overlays are therefore just `layers` in that order.

## Rows and patches

A row is `%{"id", "name", "config", "group", "disabled", "inject"}` (`EntryOptions`).
Patch semantics are a port of `applyEntryPatches` in `vendor/include/src/index.ts`:

| Patch | Behaviour |
|---|---|
| `insert:` with no `id` | rows are appended to the top level |
| `insert:` with `id` | rows are appended to that entry's `config`; the target must exist and be a group, otherwise the patch warns and is skipped. A non-list `config` is reset to `[]` first |
| any `insert` | the inserted rows are indexed immediately, so a **later** patch in the same list can target a row an earlier patch inserted |
| no `insert` | `id` is required, otherwise the patch warns and is skipped |
| unknown `id` | warns and is skipped |
| `name:` on a non-insert patch | a guard only: a mismatch warns and skips the patch, a match never rewrites the row's name |
| any other key | **replaces the whole value** for that key. There is no deep merge: a patched `config` wins entirely |

Grouping follows Cordis: a `group: true` row (builtin name `cordis:group`) holds its
children in `config`, is mounted as a `Tenon.Loader.Group` fiber, and its children are
mounted under that fiber's ctx, so unmounting a group cascades. A group is never itself
disabled; a truthy `disabled` on a group disables its descendants.

## `!!js` policy

`!!js <expr>` scalars are captured as `%{"__jsExpr" => expr}` nodes, the same shape the
DSH include produces, and are never evaluated on the BEAM.

| Row | `!!js` in `config` | `disabled: !!js` |
|---|---|---|
| collapsed (`dsh-*`) | passed through untouched | passed through untouched |
| external (`cmd`) | passed through untouched | fails loud |
| native (`module`) | fails loud | fails loud |
| group | — | fails loud |

"Fails loud" means: the row is logged as an error, is not mounted, and appears in `dump/1`
with `kind: :error` and its reason. The rest of the tree still mounts.

## Ops

| Call | Meaning |
|---|---|
| `Tenon.Loader.reload(loader)` | re-read every layer, recompose, and apply the diff |
| `Tenon.Loader.dump(loader)` | composed rows with resolved `kind`, `parent`, `disabled`, `error`, `fiber` and live fiber `status` |
| `Tenon.Loader.warnings(loader)` | patch warnings from the last composition |

`loader` is the loader's own fiber pid. The diff is by id: added rows mount, removed rows
unmount, a changed `config` is `:tenon.restart(fiber, config)` (the fiber pid survives), a
`disabled` toggle is an unmount / mount, and a changed `name`, `parent` or kind is an
unmount followed by a mount. A collapsed bridge restarts in place when only its config
changed.

## Deviations from DSH

1. **Stable generated ids.** DSH's `ensureId` assigns `Math.random()` ids to rows without
   one; a reload diff by id needs determinism, so a row without an `id` gets
   `anon:<parent>/<name>:<n>` from its position among same-named siblings.
2. **A duplicate id fails that row** (logged, skipped, in the dump) instead of throwing and
   failing the whole tree, which is the same stance as an unknown name.
3. **Config keys stay strings.** A plugin's `load/2` receives the parsed YAML as-is.
4. **Rows under a disabled group are not harvested** for collapse, so a disabled subtree
   never reaches the bridge.
5. `intercept` and `isolate` (`vendor/loader/src/config/isolate.ts`) are **ignored with a
   warning** on native, external and group rows — the kernel has one service realm. A
   collapsed row keeps them; the bridge owns them.

## Limits

No file watching: `reload/1` is explicit. No per-row `inject` (the kernel takes injected
names from the plugin module's `inject/0`). No nested include trees (`EntryTree.sep`, `a:b`
ids): one composed tree per loader.

## Tests

```
mix compile --warnings-as-errors && mix format --check-formatted
mix credo --strict && mix test        # seeds 1..3 verified
```

49 tests. `test/config_test.exs` ports the `applyEntryPatches` and YAML dialect cases,
`test/tree_test.exs` covers spec resolution, disabled inheritance, the `!!js` policy and
collapse harvesting, and `test/loader_test.exs` runs the real kernel: mount, group cascade,
reload diff, a bridge fiber speaking the wire (`test/fixtures/wire_plugin.py`), and a
trimmed DSH example composition plus its patch layer in `test/fixtures/`.
