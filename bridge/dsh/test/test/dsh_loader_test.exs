defmodule DshLoaderTest do
  use ExUnit.Case, async: false

  alias Tenon.Loader

  @tenon Path.expand("../../../..", __DIR__)
  @dsh System.get_env("DSH_REPO") || Path.expand("../deepseek-harness", @tenon)
  @dsh_home System.get_env("DSH_HOME") || Path.join(@tenon, "playground/dsh-home")
  @bin Path.join(@dsh, "apps/cli/lib/bin.js")
  @bridge Path.join(@tenon, "bridge/dsh/dist/plugin.js")
  @profile "tenon-loader"
  @boot_timeout 180_000
  @hmr_timeout 60_000

  @moduletag :dsh

  if not (File.exists?(@bin) and File.exists?(@bridge) and File.dir?(@dsh_home)) do
    @moduletag skip: "needs DSH_HOME, a built dsh (#{@bin}) and a built bridge (#{@bridge})"
  end

  defmodule Native do
    @moduledoc false

    def inject, do: []

    def load(_ctx, config) do
      {:ok, fn -> send(:dsh_loader_test, {:native_unloaded, config["tag"]}) end}
    end
  end

  setup do
    Process.register(self(), :dsh_loader_test)
    dir = Path.join(System.tmp_dir!(), "tenon-dsh-loader-#{System.unique_integer([:positive])}")
    File.mkdir_p!(dir)
    profile_dir = Path.join([@dsh_home, "profiles", @profile])

    {:ok, kernel} = :tenon.start(%{deadline: 90_000})

    on_exit(fn ->
      shutdown(kernel)
      File.rm_rf!(dir)
      File.rm_rf!(profile_dir)
    end)

    %{ctx: :tenon.root(kernel), dir: dir, profile_dir: profile_dir}
  end

  test "one config tree mounts a native row and the dsh rows as one dsh fiber", context do
    probe = write_probe(context.dir)
    mark = Path.join(context.dir, "one.mark")
    layer = write_tree(context.dir, probe, [{"tenon-probe", mark}])

    {:ok, loader} = mount(context.ctx, layer)

    dump = Loader.dump(loader)
    assert Enum.map(dump, & &1.id) == ["native", "dsh"]
    assert row(dump, "native").kind == :module
    assert row(dump, "native").status == :active

    dsh = row(dump, "dsh")
    assert dsh.kind == :collapsed
    assert dsh.rows == ["session-title", "tenon-probe"]
    assert wait_until(fn -> :tenon.status(dsh.fiber) == :active end, @boot_timeout)

    assert :tenon.svc(context.ctx, :dsh, :ping, []) == "pong"
    os_pid = :tenon.svc(context.ctx, :dsh, :pid, [])
    assert is_integer(os_pid)

    patch = File.read!(Path.join(context.profile_dir, "cordis.patch.yml"))
    assert patch =~ "id: tenon-bridge"
    assert patch =~ "fallbackMaxWords: 4"
    assert File.read!(Path.join(context.profile_dir, "package.json")) =~ "@deepseek-ai/dsh-base"

    assert wait_until(fn -> File.exists?(mark) end, @hmr_timeout),
           "the inserted dsh row never ran inside the dsh process"

    added = Path.join(context.dir, "two.mark")
    write_tree(context.dir, probe, [{"tenon-probe", mark}, {"tenon-probe-2", added}])
    :ok = Loader.reload(loader)

    reloaded = row(Loader.dump(loader), "dsh")
    assert reloaded.fiber == dsh.fiber
    assert reloaded.rows == ["session-title", "tenon-probe", "tenon-probe-2"]

    assert wait_until(fn -> File.exists?(added) end, @hmr_timeout),
           "dsh did not hot-reload the rewritten profile patch"

    assert :tenon.svc(context.ctx, :dsh, :pid, []) == os_pid

    assert :tenon.unmount(loader) == :ok
    assert_receive {:native_unloaded, "native"}, 30_000
  end

  defp write_tree(dir, probe, probes) do
    inserts =
      Enum.map_join(probes, "\n", fn {id, mark} ->
        """
        - id: #{id}
          tenon: dsh
          name: '#{probe}'
          config:
            mark: '#{mark}'
        """
      end)

    path = Path.join(dir, "cordis.yml")

    File.write!(path, """
    - id: native
      name: tenon:native
      config:
        tag: native

    - id: session-title
      tenon: dsh
      config:
        fallbackMaxWords: 4
        fallbackMaxBytes: 32
        maxTitleBytes: 80

    #{inserts}
    """)

    path
  end

  defp mount(ctx, layer) do
    :tenon.mount(ctx, %{
      module: Loader,
      id: "loader",
      config: %{
        layers: [layer],
        registry: %{"tenon:native" => %{module: Native}},
        dsh: %{
          dsh_home: @dsh_home,
          profile: @profile,
          launcher: [node_executable(), @bin],
          bridge: %{module_path: @bridge},
          env: [{"PATH", node_path()}]
        }
      }
    })
  end

  defp write_probe(dir) do
    path = Path.join(dir, "probe.mjs")

    File.write!(path, """
    import { writeFileSync } from 'node:fs'

    export const name = 'tenon-probe'

    export function apply(ctx, config) {
      writeFileSync(config.mark, String(process.pid))
    }
    """)

    path
  end

  defp row(dump, id), do: Enum.find(dump, &(&1.id == id))

  defp shutdown(kernel) do
    if Process.alive?(kernel) do
      kernel |> :tenon.tree() |> Map.get(:children, []) |> Enum.each(&:tenon.unmount(&1.pid))
      :tenon.stop(kernel)
    end
  catch
    :exit, _reason -> :ok
  end

  defp node_path, do: Path.dirname(node_executable()) <> ":" <> System.get_env("PATH", "")

  defp node_executable do
    System.find_executable("node") || Path.expand("~/.nvm/versions/node/v24.14.0/bin/node")
  end

  defp wait_until(fun, timeout) do
    poll(fun, System.monotonic_time(:millisecond) + timeout)
  end

  defp poll(fun, deadline) do
    cond do
      fun.() -> true
      System.monotonic_time(:millisecond) > deadline -> false
      true -> :timer.sleep(100) && poll(fun, deadline)
    end
  end
end
