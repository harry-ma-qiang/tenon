defmodule BridgeTest do
  use ExUnit.Case, async: false

  @tenon Path.expand("../../../..", __DIR__)
  @bridge Path.join(@tenon, "bridge/dsh")
  @dsh System.get_env("DSH_REPO") || Path.expand("../deepseek-harness", @tenon)
  @dsh_home System.get_env("DSH_HOME") || Path.join(@tenon, "playground/dsh-home")
  @bin Path.join(@dsh, "apps/cli/lib/bin.js")
  @guard Path.join(__DIR__, "guard.py")
  @boot_timeout 180_000

  setup_all do
    build_bridge()
    ctx = kernel()
    mount_guard(ctx)
    {fiber, boot_ms} = mount_dsh(ctx)
    IO.puts("dsh booted as one tenon plugin in #{boot_ms} ms")
    {:ok, ctx: ctx, fiber: fiber}
  end

  test "dsh boots as one tenon plugin and answers on the mirrored dsh service", context do
    assert :tenon.status(context.fiber) == :active
    assert :tenon.svc(context.ctx, :dsh, :ping, []) == "pong"

    tools = :tenon.svc(context.ctx, :dsh, :"tools.list", [])
    assert is_list(tools)
    assert tools != []
    assert Enum.all?(tools, &is_map/1)
    assert Enum.any?(tools, &(&1["name"] == "tenon_echo"))

    assert is_list(:tenon.svc(context.ctx, :dsh, :"sessions.list", []))
    assert is_list(:tenon.svc(context.ctx, :dsh, :"agents.list", []))
  end

  test "the manifest installs one mirror per declared event", context do
    mirrors = :tenon.svc(context.ctx, :dsh, :mirrors, [])

    assert Enum.sort(Enum.map(mirrors, & &1["name"])) ==
             ["session/created", "session/event", "tools/pre-execute"]

    assert Enum.find(mirrors, &(&1["name"] == "tools/pre-execute"))["mode"] == "call"
    assert Enum.find(mirrors, &(&1["name"] == "session/created"))["mode"] == "emit"
  end

  test "a tenon python hook allows and denies dsh tool calls", context do
    allowed_before = :tenon.svc(context.ctx, :guard, :allowed, [])
    denied_before = :tenon.svc(context.ctx, :guard, :denied, [])

    allowed = execute(context.ctx, "tenon_echo", %{"text" => "hi"})

    assert allowed["ok"] == true
    assert allowed["content"] == "hi"
    assert :tenon.svc(context.ctx, :guard, :allowed, []) == allowed_before + 1

    denied = execute(context.ctx, "tenon_echo", %{"text" => "rm -rf /tmp/nope"})

    assert denied["ok"] == false
    assert denied["error"] =~ "tenon guard"
    assert :tenon.svc(context.ctx, :guard, :denied, []) == denied_before + 1
  end

  test "a dsh session/created emit reaches a tenon hook", context do
    created = :tenon.svc(context.ctx, :dsh, :"sessions.create", [])

    assert is_binary(created["id"])
    assert wait_until(fn -> created["id"] in :tenon.svc(context.ctx, :guard, :sessions, []) end)
  end

  test "unmount ends the dsh os process" do
    ctx = kernel()
    {fiber, _} = mount_dsh(ctx)
    os_pid = :tenon.svc(ctx, :dsh, :pid, [])
    assert is_integer(os_pid)
    assert alive?(os_pid)

    assert :tenon.unmount(fiber) == :ok

    assert wait_until(fn -> not alive?(os_pid) end, 30_000)
  end

  defp execute(ctx, tool, input) do
    :tenon.svc(ctx, :dsh, :"tools.execute", [tool, input])
  end

  defp kernel do
    {:ok, k} = :tenon.start(%{deadline: 90_000})
    on_exit(fn -> shutdown(k) end)
    :tenon.root(k)
  end

  defp shutdown(kernel) do
    if Process.alive?(kernel) do
      kernel |> :tenon.tree() |> Map.get(:children, []) |> Enum.each(&:tenon.unmount(&1.pid))
      :tenon.stop(kernel)
    end
  end

  defp mount_guard(ctx) do
    {:ok, fiber} =
      :tenon.mount(ctx, %{
        cmd: String.to_charlist(executable("python3")),
        args: [String.to_charlist(@guard)],
        env: [{~c"PYTHONPATH", String.to_charlist(Path.join(@tenon, "sdk/py"))}],
        config: %{}
      })

    assert :tenon.status(fiber) == :active
    fiber
  end

  defp mount_dsh(ctx) do
    started = System.monotonic_time(:millisecond)

    {:ok, fiber} =
      :tenon.mount(ctx, %{
        cmd: String.to_charlist(node_executable()),
        args: [String.to_charlist(@bin), ~c"--profile", ~c"tenon"],
        env: [
          {~c"DSH_HOME", String.to_charlist(@dsh_home)},
          {~c"PATH", String.to_charlist(node_path())}
        ],
        config: %{}
      })

    assert wait_until(fn -> :tenon.status(fiber) == :active end, @boot_timeout),
           "dsh never became active, status #{inspect(:tenon.status(fiber))}"

    {fiber, System.monotonic_time(:millisecond) - started}
  end

  defp build_bridge do
    pnpm = executable("pnpm", ["~/.local/share/pnpm/pnpm"])
    unless File.dir?(Path.join(@bridge, "node_modules")), do: run!(pnpm, ["install"])
    run!(pnpm, ["run", "build"])
  end

  defp run!(exe, args) do
    env = [{"PATH", node_path()}]

    case System.cmd(exe, args, cd: @bridge, env: env, stderr_to_stdout: true) do
      {_out, 0} -> :ok
      {out, code} -> raise "#{exe} #{Enum.join(args, " ")} failed with #{code}\n#{out}"
    end
  end

  defp node_path do
    Path.dirname(node_executable()) <> ":" <> System.get_env("PATH", "")
  end

  defp node_executable do
    executable("node", ["~/.nvm/versions/node/v24.14.0/bin/node"])
  end

  defp executable(name, fallbacks \\ []) do
    found =
      System.find_executable(name) ||
        Enum.find_value(fallbacks, fn path ->
          expanded = Path.expand(path)
          if File.exists?(expanded), do: expanded
        end)

    found || raise "#{name} not found on this machine"
  end

  defp alive?(os_pid) do
    match?({_, 0}, System.cmd("kill", ["-0", to_string(os_pid)], stderr_to_stdout: true))
  end

  defp wait_until(fun, timeout \\ 10_000) do
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
