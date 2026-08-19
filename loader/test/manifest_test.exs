defmodule Tenon.Loader.ManifestTest do
  use ExUnit.Case, async: false

  import ExUnit.CaptureLog

  alias Tenon.Loader
  alias Tenon.Loader.Manifest
  alias Tenon.Loader.Probe

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

  defp plugin(dir, entry, manifest) do
    path = Path.join(dir, entry)
    File.mkdir_p!(path)
    File.write!(Path.join(path, "manifest.json"), Jason.encode!(manifest))
    path
  end

  defp wire(name, version) do
    %{
      "name" => name,
      "version" => version,
      "hash" => "sha256:#{name}",
      "cmd" => System.find_executable("python3"),
      "args" => [fixture("wire_plugin.py")],
      "protocol" => "wire/1"
    }
  end

  @tag :tmp_dir
  test "reads one row per manifest, by name and by name@version", %{tmp_dir: dir} do
    plugin(dir, "echo@1.0.0", wire("echo", "1.0.0"))
    plugin(dir, "tally@0.2.1", wire("tally", "0.2.1"))

    registry = Manifest.load(dir)
    assert Enum.sort(Map.keys(registry)) == ["echo", "echo@1.0.0", "tally", "tally@0.2.1"]
    assert registry["echo"] == registry["echo@1.0.0"]
    assert registry["echo"].cmd == System.find_executable("python3")
    assert Manifest.list(dir) |> Enum.map(& &1["version"]) == ["1.0.0", "0.2.1"]
  end

  @tag :tmp_dir
  test "resolves a relative cmd against the plugin's own directory", %{tmp_dir: dir} do
    path = plugin(dir, "rel@1", Map.put(wire("rel", "1"), "cmd", "./bin/run"))
    assert Manifest.load(dir)["rel"].cmd == Path.join(path, "bin/run")
  end

  @tag :tmp_dir
  test "refuses a manifest that is incomplete or speaks another protocol", %{tmp_dir: dir} do
    plugin(dir, "no-version@1", Map.delete(wire("nov", "1"), "version"))
    plugin(dir, "no-cmd@1", Map.delete(wire("nocmd", "1"), "cmd"))
    plugin(dir, "alien@1", Map.put(wire("alien", "1"), "protocol", "grpc"))
    plugin(dir, "broken@1", %{})
    File.write!(Path.join(dir, "broken@1/manifest.json"), "{not json")
    File.mkdir_p!(Path.join(dir, "empty@1"))

    log = capture_log(fn -> assert Manifest.load(dir) == %{} end)
    assert log =~ "no_version"
    assert log =~ "no_cmd"
    assert log =~ "unknown_protocol"
  end

  @tag :tmp_dir
  test "a profile row resolves against a manifest and mounts", %{ctx: ctx, tmp_dir: dir} do
    plugins = Path.join(dir, "plugins")
    plugin(plugins, "echo@1.0.0", wire("echo", "1.0.0"))
    layer = Path.join(dir, "tenon.yml")
    File.write!(layer, "- id: one\n  name: echo\n- id: two\n  name: echo@1.0.0\n")

    {:ok, loader} =
      :tenon.mount(ctx, %{
        module: Loader,
        id: "loader",
        config: %{layers: [layer], manifests: [plugins]}
      })

    assert :tenon.status(loader) == :active
    dump = Loader.dump(loader)
    assert Enum.map(dump, & &1.id) == ["one", "two"]
    assert Enum.all?(dump, &(&1.kind == :external and &1.status == :active))
  end

  @tag :tmp_dir
  test "an explicit registry row wins over a manifest of the same name", %{
    ctx: ctx,
    tmp_dir: dir
  } do
    plugins = Path.join(dir, "plugins")
    plugin(plugins, "echo@1.0.0", wire("echo", "1.0.0"))
    layer = Path.join(dir, "tenon.yml")
    File.write!(layer, "- id: one\n  name: echo\n")

    {:ok, loader} =
      :tenon.mount(ctx, %{
        module: Loader,
        id: "loader",
        config: %{
          layers: [layer],
          manifests: [plugins],
          registry: %{"echo" => %{module: Tenon.Loader.Echo}}
        }
      })

    assert Loader.dump(loader) |> hd() |> Map.get(:kind) == :module
  end
end
