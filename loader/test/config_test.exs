defmodule Tenon.Loader.ConfigTest do
  use ExUnit.Case, async: true

  alias Tenon.Loader.Config

  defp fixture(name), do: Path.join([__DIR__, "fixtures", name])

  defp patch(data, patches) do
    {rows, warnings} = Config.apply_entry_patches(data, patches)
    {rows, warnings}
  end

  describe "apply_entry_patches" do
    test "insert without id appends at top level" do
      {rows, warnings} =
        patch([%{"id" => "a", "name" => "x"}], [%{"insert" => [%{"id" => "b", "name" => "y"}]}])

      assert Enum.map(rows, & &1["id"]) == ["a", "b"]
      assert warnings == []
    end

    test "insert with id appends into that group's config" do
      data = [%{"id" => "g", "name" => "cordis:group", "group" => true, "config" => []}]
      {rows, []} = patch(data, [%{"id" => "g", "insert" => [%{"id" => "b", "name" => "y"}]}])

      assert [%{"config" => [%{"id" => "b"}]}] = rows
    end

    test "insert into a group whose config is not a list resets it" do
      data = [%{"id" => "g", "name" => "cordis:group", "group" => true, "config" => "junk"}]
      {rows, []} = patch(data, [%{"id" => "g", "insert" => [%{"id" => "b"}]}])

      assert [%{"config" => [%{"id" => "b"}]}] = rows
    end

    test "insert into an unknown id warns and skips" do
      {rows, warnings} = patch([], [%{"id" => "nope", "insert" => [%{"id" => "b"}]}])

      assert rows == []
      assert warnings == [~s(patch insert: entry "nope" not found)]
    end

    test "insert into a non-group warns and skips" do
      data = [%{"id" => "a", "name" => "x"}]
      {rows, warnings} = patch(data, [%{"id" => "a", "insert" => [%{"id" => "b"}]}])

      assert rows == data
      assert warnings == [~s(patch insert: entry "a" is not a group)]
    end

    test "a later patch targets a row an earlier patch inserted" do
      patches = [
        %{"insert" => [%{"id" => "b", "name" => "y", "config" => %{"n" => 1}}]},
        %{"id" => "b", "config" => %{"n" => 2}}
      ]

      {rows, []} = patch([], patches)
      assert [%{"id" => "b", "config" => %{"n" => 2}}] = rows
    end

    test "a later patch targets a row inserted into a group" do
      data = [%{"id" => "g", "group" => true, "config" => []}]

      patches = [
        %{"id" => "g", "insert" => [%{"id" => "b", "name" => "y"}]},
        %{"id" => "b", "disabled" => true}
      ]

      {rows, []} = patch(data, patches)
      assert [%{"config" => [%{"id" => "b", "disabled" => true}]}] = rows
    end

    test "a non-insert patch without an id warns" do
      {rows, warnings} = patch([], [%{"config" => %{}}])

      assert rows == []
      assert warnings == ["patch: id is required for non-insert patches"]
    end

    test "an unknown id warns and skips" do
      {rows, warnings} = patch([%{"id" => "a"}], [%{"id" => "z", "config" => %{}}])

      assert rows == [%{"id" => "a"}]
      assert warnings == [~s(patch: entry "z" not found)]
    end

    test "a patch replaces whole values per key instead of deep merging" do
      data = [%{"id" => "a", "config" => %{"keep" => 1, "drop" => 2}}]
      {rows, []} = patch(data, [%{"id" => "a", "config" => %{"keep" => 9}}])

      assert [%{"config" => %{"keep" => 9}}] = rows
    end

    test "a name mismatch warns and skips" do
      data = [%{"id" => "a", "name" => "x"}]
      {rows, warnings} = patch(data, [%{"id" => "a", "name" => "other", "config" => %{"n" => 1}}])

      assert rows == data
      assert warnings == [~s(patch: name mismatch for "a", skipping)]
    end

    test "a matching name is a guard only and never rewrites the row name" do
      data = [%{"id" => "a", "name" => "x"}]
      {rows, []} = patch(data, [%{"id" => "a", "name" => "x", "disabled" => true}])

      assert [%{"name" => "x", "disabled" => true}] = rows
    end

    test "disabled is set and cleared like any other key" do
      data = [%{"id" => "a", "disabled" => true}]
      {rows, []} = patch(data, [%{"id" => "a", "disabled" => false}])

      assert [%{"disabled" => false}] = rows
    end

    test "patches are applied in list order, last write wins" do
      patches = [
        %{"insert" => [%{"id" => "a", "name" => "x"}]},
        %{"id" => "a", "config" => %{"n" => 1}},
        %{"id" => "a", "config" => %{"n" => 2}}
      ]

      {rows, []} = patch([], patches)
      assert [%{"config" => %{"n" => 2}}] = rows
    end
  end

  describe "yaml" do
    test "captures !!js scalars as expression nodes" do
      rows = Config.read(fixture("dsh-cordis.yml"))
      persistence = Enum.find(rows, &(&1["id"] == "persistence"))
      fs = Enum.find(rows, &(&1["id"] == "fs-local"))
      telemetry = Enum.find(rows, &(&1["id"] == "telemetry"))

      assert persistence["config"]["compression"] == %{
               "__jsExpr" => "process.env.DSH_SNAPSHOT === undefined ? 'zstd' : 'none'"
             }

      assert fs["config"]["cwd"] == %{"__jsExpr" => "process.cwd()"}
      assert telemetry["disabled"] == %{"__jsExpr" => "process.platform === 'win32'"}

      assert telemetry["config"]["mode"] == %{
               "__jsExpr" => "process.env.DSH_TELEMETRY_MODE || 'DISABLED'"
             }
    end

    test "captures single quoted and list-item !!js scalars" do
      rows =
        Config.parse!("""
        - id: a
          name: x
          config:
            one: !!js 'a ?? ''b'''
            two:
              - !!js first()
              - plain
        """)

      assert [%{"config" => %{"one" => one, "two" => [two, "plain"]}}] = rows
      assert one == %{"__jsExpr" => "a ?? 'b'"}
      assert two == %{"__jsExpr" => "first()"}
    end

    test "a plain !!js scalar drops a trailing comment" do
      rows = Config.parse!("- id: a\n  config:\n    v: !!js process.cwd() # why\n")
      assert [%{"config" => %{"v" => %{"__jsExpr" => "process.cwd()"}}}] = rows
    end

    test "a !!js block scalar is rejected" do
      assert_raise ArgumentError, fn -> Config.parse!("- id: a\n  config:\n    v: !!js |\n") end
    end

    test "an empty layer is an empty row list" do
      assert Config.parse!("") == []
      assert Config.parse!("# only a comment\n") == []
    end

    test "a non-list document is rejected" do
      assert_raise ArgumentError, fn -> Config.parse!("a: 1\n") end
    end
  end

  describe "normalize" do
    test "generates stable ids scoped to the parent group" do
      rows =
        Config.normalize([
          %{"name" => "probe"},
          %{"name" => "probe"},
          %{"id" => "kept", "name" => "probe"},
          %{"name" => "g", "group" => true, "config" => [%{"name" => "probe"}]}
        ])

      assert Enum.map(rows, & &1["id"]) == [
               "anon:probe:0",
               "anon:probe:1",
               "kept",
               "anon:g:0"
             ]

      assert [%{"id" => "anon:anon:g:0/probe:0"}] = List.last(rows)["config"]
      assert Config.normalize(rows) == rows
    end
  end

  describe "compose" do
    test "an entries layer under a patch layer, applied in order" do
      {rows, warnings} =
        Config.compose([fixture("dsh-cordis.yml"), fixture("dsh-cordis.patch.yml")])

      assert warnings == []
      ids = Enum.map(rows, & &1["id"])
      assert List.last(ids) == "policy"
      assert Enum.find(rows, &(&1["id"] == "bash"))["config"] == %{"timeoutMs" => 30_000}
      assert Enum.find(rows, &(&1["id"] == "fs-local"))["disabled"] == true
    end

    test "layer order decides the winner" do
      base = [%{"insert" => [%{"id" => "a", "name" => "x", "config" => %{"n" => 0}}]}]
      first = [%{"id" => "a", "config" => %{"n" => 1}}]
      second = [%{"id" => "a", "config" => %{"n" => 2}}]

      {rows, []} =
        Config.compose([{:patch, base}, {:patch, first}, {:patch, second}])

      assert [%{"config" => %{"n" => 2}}] = rows

      {rows, []} =
        Config.compose([{:patch, base}, {:patch, second}, {:patch, first}])

      assert [%{"config" => %{"n" => 1}}] = rows
    end

    test "a .patch.yml path is a patch layer and any other path is an entry list" do
      assert [%{"insert" => _}] = Config.layer_patches(fixture("dsh-cordis.yml"))
      assert [%{"id" => "bash"} | _] = Config.layer_patches(fixture("dsh-cordis.patch.yml"))
    end

    test "warnings from a patch layer are returned" do
      {_rows, warnings} = Config.compose([{:patch, [%{"id" => "ghost", "config" => %{}}]}])
      assert warnings == [~s(patch: entry "ghost" not found)]
    end
  end
end
