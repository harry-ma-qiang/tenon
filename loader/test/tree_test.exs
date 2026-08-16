defmodule Tenon.Loader.TreeTest do
  use ExUnit.Case, async: true

  import ExUnit.CaptureLog

  alias Tenon.Loader.Config
  alias Tenon.Loader.Group
  alias Tenon.Loader.Tree

  @registry %{
    "probe" => %{module: Tenon.Loader.Echo},
    "shell" => %{cmd: "/bin/echo", args: ["hi"], env: []}
  }

  defp build(rows, opts \\ %{}) do
    Tree.build(Config.normalize(rows), Map.put_new(opts, :registry, @registry))
  end

  defp quiet(rows, opts \\ %{}) do
    {built, _log} = with_log(fn -> build(rows, opts) end)
    built
  end

  defp node(built, id), do: Enum.find(built.nodes, &(&1.id == id))

  defp collapse do
    [{"@deepseek-ai/dsh-", fn rows -> %{cmd: "/bin/echo", config: rows} end}]
  end

  test "resolves module, external and group rows" do
    built =
      build([
        %{"id" => "a", "name" => "probe", "config" => %{"tag" => 1}},
        %{"id" => "b", "name" => "shell"},
        %{"id" => "g", "name" => "cordis:group", "group" => true, "config" => []}
      ])

    assert node(built, "a").kind == :module
    assert node(built, "a").spec == %{module: Tenon.Loader.Echo, config: %{"tag" => 1}, id: "a"}
    assert node(built, "b").kind == :external

    assert node(built, "b").spec == %{
             cmd: "/bin/echo",
             args: ["hi"],
             env: [],
             config: nil,
             id: "b"
           }

    assert node(built, "g").kind == :group
    assert node(built, "g").spec == %{module: Group, config: %{id: "g"}, id: "g"}
  end

  test "an unknown name fails loud and stays in the tree as an error row" do
    {built, log} = with_log(fn -> build([%{"id" => "a", "name" => "ghost"}]) end)

    assert node(built, "a").kind == :error
    assert node(built, "a").error == {:unknown_name, "ghost"}
    assert node(built, "a").spec == nil
    assert log =~ "row \"a\" (ghost) failed"
  end

  test "a duplicate id fails loud" do
    built = quiet([%{"id" => "a", "name" => "probe"}, %{"id" => "a", "name" => "probe"}])

    assert Enum.map(built.nodes, & &1.kind) == [:module, :error]
    assert Enum.at(built.nodes, 1).error == {:duplicate_id, "a"}
  end

  test "children of a group are nested and carry the parent id" do
    built =
      build([
        %{
          "id" => "g",
          "group" => true,
          "name" => "cordis:group",
          "config" => [%{"id" => "c", "name" => "probe"}]
        }
      ])

    assert Enum.map(built.nodes, & &1.id) == ["g", "c"]
    assert node(built, "c").parent == "g"
    assert node(built, "g").parent == nil
  end

  test "a disabled group still mounts but disables its descendants" do
    built =
      build([
        %{
          "id" => "g",
          "group" => true,
          "name" => "cordis:group",
          "disabled" => true,
          "config" => [
            %{"id" => "c", "name" => "probe"},
            %{
              "id" => "g2",
              "group" => true,
              "name" => "cordis:group",
              "config" => [%{"id" => "d", "name" => "probe"}]
            }
          ]
        }
      ])

    assert node(built, "g").disabled == false
    assert node(built, "g2").disabled == false
    assert node(built, "c").disabled == true
    assert node(built, "d").disabled == true
  end

  test "a disabled leaf keeps its spec but is not mountable" do
    built = build([%{"id" => "a", "name" => "probe", "disabled" => true}])

    assert node(built, "a").disabled == true
    assert node(built, "a").spec != nil
  end

  test "a !!js value in a native row config fails loud" do
    built =
      quiet([%{"id" => "a", "name" => "probe", "config" => %{"k" => %{"__jsExpr" => "x()"}}}])

    assert node(built, "a").error == :js_expr_unsupported
  end

  test "a !!js disabled fails loud on native, external and group rows" do
    js = %{"__jsExpr" => "x()"}

    built =
      quiet([
        %{"id" => "a", "name" => "probe", "disabled" => js},
        %{"id" => "b", "name" => "shell", "disabled" => js},
        %{
          "id" => "g",
          "name" => "cordis:group",
          "group" => true,
          "disabled" => js,
          "config" => []
        }
      ])

    assert Enum.map(built.nodes, & &1.kind) == [:error, :error, :error]
    assert node(built, "a").error == :js_expr_unsupported
    assert node(built, "b").error == :js_disabled_unsupported
    assert node(built, "g").error == :js_disabled_unsupported
  end

  test "a !!js value in an external row config is passed through untouched" do
    js = %{"__jsExpr" => "process.cwd()"}
    built = build([%{"id" => "b", "name" => "shell", "config" => %{"cwd" => js}}])

    assert node(built, "b").kind == :external
    assert node(built, "b").spec.config == %{"cwd" => js}
  end

  test "intercept and isolate are ignored with a warning" do
    rows = [%{"id" => "a", "name" => "probe", "intercept" => %{}, "isolate" => %{}}]
    {built, log} = with_log(fn -> build(rows) end)

    assert node(built, "a").kind == :module
    assert log =~ "ignores intercept, isolate"
  end

  test "collapse: matching rows leave the tree and arrive as one list" do
    built =
      build(
        [
          %{"id" => "a", "name" => "probe"},
          %{"id" => "llm", "name" => "@deepseek-ai/dsh-llm-deepseek"},
          %{"id" => "fs", "name" => "@deepseek-ai/dsh-fs-local", "disabled" => true}
        ],
        %{collapse: collapse()}
      )

    assert Enum.map(built.nodes, & &1.id) == ["a"]
    assert [{"@deepseek-ai/dsh-", rows, _fun}] = built.collapse
    assert Enum.map(rows, & &1["id"]) == ["llm", "fs"]
    assert Enum.at(rows, 1)["disabled"] == true
  end

  test "collapse: rows under a disabled group are not harvested" do
    built =
      build(
        [
          %{
            "id" => "g",
            "group" => true,
            "name" => "cordis:group",
            "disabled" => true,
            "config" => [%{"id" => "llm", "name" => "@deepseek-ai/dsh-llm-deepseek"}]
          }
        ],
        %{collapse: collapse()}
      )

    assert [{"@deepseek-ai/dsh-", [], _fun}] = built.collapse
  end

  test "collapse: the first matching prefix wins" do
    prefixes = [{"dsh-", fn rows -> %{cmd: "a", config: rows} end} | collapse()]

    built =
      build(
        [
          %{"id" => "x", "name" => "dsh-thing"},
          %{"id" => "y", "name" => "@deepseek-ai/dsh-x"}
        ],
        %{collapse: prefixes}
      )

    assert built.nodes == []
    assert [{"dsh-", short, _}, {"@deepseek-ai/dsh-", long, _}] = built.collapse
    assert Enum.map(short, & &1["id"]) == ["x"]
    assert Enum.map(long, & &1["id"]) == ["y"]
  end

  test "collapse: an unmatched name still fails loud" do
    built = quiet([%{"id" => "z", "name" => "other-thing"}], %{collapse: collapse()})

    assert node(built, "z").error == {:unknown_name, "other-thing"}
  end
end
