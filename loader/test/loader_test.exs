defmodule Tenon.LoaderTest do
  use ExUnit.Case, async: false

  import ExUnit.CaptureLog

  alias Tenon.Loader
  alias Tenon.Loader.Config
  alias Tenon.Loader.Probe

  @probe %{"probe" => %{module: Tenon.Loader.Echo}}

  setup do
    :ok = Probe.reset()
    {:ok, kernel} = :tenon.start_link()
    on_exit(fn -> stop(kernel) end)
    %{kernel: kernel, ctx: :tenon.root(kernel)}
  end

  defp stop(kernel) do
    :tenon.stop(kernel)
  catch
    :exit, _reason -> :ok
  end

  defp fixture(name), do: Path.join([__DIR__, "fixtures", name])

  defp write(dir, name, body) do
    path = Path.join(dir, name)
    File.write!(path, body)
    path
  end

  defp mount(ctx, config) do
    :tenon.mount(ctx, %{module: Loader, config: config, id: "loader"})
  end

  defp loads, do: for({:load, config} <- Probe.events(), do: config["tag"])
  defp unloads, do: for({:unload, config} <- Probe.events(), do: config["tag"])

  defp row(dump, id), do: Enum.find(dump, &(&1.id == id))

  test "mounts the composed tree under the loader fiber", %{ctx: ctx, kernel: kernel} do
    {:ok, loader} = mount(ctx, %{layers: [fixture("tree.yml")], registry: @probe})

    assert :tenon.status(loader) == :active
    assert loads() == ["a", "b", "c"]

    dump = Loader.dump(loader)
    assert Enum.map(dump, & &1.id) == ["alpha", "grp", "beta", "gamma"]
    assert row(dump, "grp").kind == :group
    assert row(dump, "beta").parent == "grp"
    assert Enum.all?(dump, &(&1.status == :active))

    tree = :tenon.tree(kernel)
    [loader_node] = tree.children
    assert Enum.map(loader_node.children, & &1.id) == ["alpha", "grp"]

    assert [%{id: "grp", children: children}] =
             Enum.filter(loader_node.children, &(&1.id == "grp"))

    assert Enum.map(children, & &1.id) == ["beta", "gamma"]
  end

  test "unmounting the loader cascades through groups", %{ctx: ctx} do
    {:ok, loader} = mount(ctx, %{layers: [fixture("tree.yml")], registry: @probe})
    :ok = :tenon.unmount(loader)

    assert Enum.sort(unloads()) == ["a", "b", "c"]
  end

  @tag :tmp_dir
  test "reload restarts a row whose config changed", %{ctx: ctx, tmp_dir: dir} do
    path = write(dir, "cordis.yml", "- id: a\n  name: probe\n  config:\n    tag: one\n")
    {:ok, loader} = mount(ctx, %{layers: [path], registry: @probe})

    before = row(Loader.dump(loader), "a").fiber
    write(dir, "cordis.yml", "- id: a\n  name: probe\n  config:\n    tag: two\n")
    :ok = Loader.reload(loader)

    assert row(Loader.dump(loader), "a").fiber == before
    assert loads() == ["one", "two"]
    assert unloads() == ["one"]
  end

  @tag :tmp_dir
  test "reload mounts added rows and unmounts removed rows", %{ctx: ctx, tmp_dir: dir} do
    path = write(dir, "cordis.yml", "- id: a\n  name: probe\n  config:\n    tag: a\n")
    {:ok, loader} = mount(ctx, %{layers: [path], registry: @probe})

    write(dir, "cordis.yml", "- id: b\n  name: probe\n  config:\n    tag: b\n")
    :ok = Loader.reload(loader)

    assert loads() == ["a", "b"]
    assert unloads() == ["a"]
    assert Enum.map(Loader.dump(loader), & &1.id) == ["b"]
  end

  @tag :tmp_dir
  test "reload follows a disabled toggle", %{ctx: ctx, tmp_dir: dir} do
    enabled = "- id: a\n  name: probe\n  config:\n    tag: a\n"
    off = "- id: a\n  name: probe\n  disabled: true\n  config:\n    tag: a\n"
    path = write(dir, "cordis.yml", enabled)
    {:ok, loader} = mount(ctx, %{layers: [path], registry: @probe})

    write(dir, "cordis.yml", off)
    :ok = Loader.reload(loader)
    assert unloads() == ["a"]
    assert row(Loader.dump(loader), "a").fiber == nil
    assert row(Loader.dump(loader), "a").disabled == true

    write(dir, "cordis.yml", enabled)
    :ok = Loader.reload(loader)
    assert loads() == ["a", "a"]
    assert row(Loader.dump(loader), "a").status == :active
  end

  @tag :tmp_dir
  test "reload unmounts a whole group and its children", %{ctx: ctx, tmp_dir: dir} do
    group = """
    - id: g
      name: cordis:group
      group: true
      config:
        - id: c
          name: probe
          config:
            tag: c
    """

    path = write(dir, "cordis.yml", group)
    {:ok, loader} = mount(ctx, %{layers: [path], registry: @probe})
    assert loads() == ["c"]

    write(dir, "cordis.yml", "- id: a\n  name: probe\n  config:\n    tag: a\n")
    :ok = Loader.reload(loader)

    assert unloads() == ["c"]
    assert Enum.map(Loader.dump(loader), & &1.id) == ["a"]
  end

  test "an unknown name fails loud without stopping the rest of the tree", %{ctx: ctx} do
    yaml = "- id: a\n  name: probe\n  config:\n    tag: a\n- id: z\n  name: ghost\n"

    layers = [{:entries, Config.parse!(yaml)}]

    {loader, log} =
      with_log(fn ->
        {:ok, pid} = mount(ctx, %{layers: layers, registry: @probe})
        pid
      end)

    assert log =~ "unknown_name"

    assert :tenon.status(loader) == :active
    assert loads() == ["a"]

    dump = Loader.dump(loader)
    assert row(dump, "z").kind == :error
    assert row(dump, "z").error == {:unknown_name, "ghost"}
    assert row(dump, "z").fiber == nil
    assert row(dump, "a").status == :active
  end

  test "patch warnings are kept for inspection", %{ctx: ctx} do
    layers = [{:patch, [%{"id" => "ghost", "config" => %{}}]}]

    {loader, _log} =
      with_log(fn ->
        {:ok, pid} = mount(ctx, %{layers: layers, registry: @probe})
        pid
      end)

    assert Loader.warnings(loader) == [~s(patch: entry "ghost" not found)]
  end

  describe "collapse" do
    defp bridge(rows) do
      %{
        cmd: System.find_executable("python3"),
        args: [Path.join([__DIR__, "fixtures", "wire_plugin.py"])],
        env: [],
        config: %{"rows" => rows}
      }
    end

    defp dsh_config(layers) do
      %{layers: layers, registry: @probe, collapse: [{"@deepseek-ai/dsh-", &bridge/1}]}
    end

    @tag :tmp_dir
    test "dsh rows collapse into one external fiber", %{ctx: ctx, tmp_dir: dir} do
      yaml = """
      - id: a
        name: probe
        config:
          tag: a

      - id: llm
        name: '@deepseek-ai/dsh-llm-deepseek'
        config:
          thinking: enabled

      - id: fs
        name: '@deepseek-ai/dsh-fs-local'
        config:
          cwd: !!js process.cwd()
      """

      path = write(dir, "cordis.yml", yaml)
      {:ok, loader} = mount(ctx, dsh_config([path]))

      dump = Loader.dump(loader)
      assert Enum.map(dump, & &1.id) == ["a", "collapse:@deepseek-ai/dsh-"]

      collapsed = row(dump, "collapse:@deepseek-ai/dsh-")
      assert collapsed.kind == :collapsed
      assert collapsed.rows == ["llm", "fs"]
      assert collapsed.status == :active

      write(dir, "cordis.yml", String.replace(yaml, "thinking: enabled", "thinking: off"))
      :ok = Loader.reload(loader)

      reloaded = row(Loader.dump(loader), "collapse:@deepseek-ai/dsh-")
      assert reloaded.fiber == collapsed.fiber
      assert :tenon.status(reloaded.fiber) == :active
    end

    test "a dsh composition parses, collapses and mounts from the shipped fixture", %{ctx: ctx} do
      layers = [fixture("dsh-cordis.yml"), fixture("dsh-cordis.patch.yml")]
      registry = Map.put(@probe, "tenon:policy", %{module: Tenon.Loader.Echo})
      {:ok, loader} = mount(ctx, %{dsh_config(layers) | registry: registry})

      dump = Loader.dump(loader)
      assert Enum.map(dump, & &1.id) == ["policy", "collapse:@deepseek-ai/dsh-"]
      assert row(dump, "policy").status == :active

      collapsed = row(dump, "collapse:@deepseek-ai/dsh-")
      assert collapsed.status == :active
      assert "telemetry" in collapsed.rows
    end

    @tag :tmp_dir
    test "the built-in dsh target writes a profile and mounts one fiber", %{
      ctx: ctx,
      tmp_dir: dir
    } do
      home = Path.join(dir, "home")
      source = write(dir, "cordis.yml", File.read!(fixture("dsh-collapse.yml")))

      config = %{
        layers: [source],
        registry: %{"tenon:policy" => %{module: Tenon.Loader.Echo}},
        dsh: %{
          dsh_home: home,
          launcher: [System.find_executable("python3"), fixture("wire_plugin.py")],
          bridge: %{module_path: fixture("bridge.js")}
        }
      }

      {:ok, loader} = mount(ctx, config)
      dump = Loader.dump(loader)

      assert Enum.map(dump, & &1.id) == ["policy", "dsh"]
      dsh = row(dump, "dsh")
      assert dsh.kind == :collapsed
      assert dsh.status == :active
      assert dsh.rows == ["session-title", "fs-local", "hello"]

      patch = Path.join([home, "profiles", "tenon", "cordis.patch.yml"])
      assert File.read!(patch) =~ "fallbackMaxWords: 3"
      assert File.read!(Path.join([home, "profiles", "tenon", "package.json"])) =~ "dsh-base"

      write(dir, "cordis.yml", String.replace(File.read!(source), "Words: 3", "Words: 7"))
      :ok = Loader.reload(loader)

      assert File.read!(patch) =~ "fallbackMaxWords: 7"
      reloaded = row(Loader.dump(loader), "dsh")
      assert reloaded.fiber == dsh.fiber
      assert reloaded.status == :active
    end
  end
end
