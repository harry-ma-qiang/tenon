defmodule SdkTest do
  use ExUnit.Case, async: false

  @sdk Path.expand("../..", __DIR__)
  @scripts %{py: Path.join(@sdk, "py/example.py"), ts: Path.join(@sdk, "ts/dist/example.js")}
  @langs [:py, :ts]
  @cap 65_536

  setup_all do
    build_typescript()
    :ok
  end

  test "both SDKs answer the same service methods" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})

      assert :tenon.svc(ctx, :demo, :ping, []) == "pong"
      assert :tenon.svc(ctx, :demo, :add, [2, 3]) == 5
      assert :tenon.svc(ctx, :demo, :getenv, ["TENON_MAX_FRAME"]) == "65536"
      assert :tenon.svc(ctx, :demo, :getenv, ["TENON_KERNEL_DEADLINE"]) == "30000"
      assert :tenon.svc(ctx, :demo, :unknown, []) == {:error, "unknown method unknown"}
    end
  end

  test "both SDKs block a dangerous command without running the terminal" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})
      me = self()
      terminal = fn request -> send(me, {:terminal, request}) end

      result = :tenon.call(ctx, :"tools/execute", [%{"cmd" => "rm -rf /"}], terminal)

      assert result == %{"status" => "blocked", "by" => "demo", "cmd" => "rm -rf /"}
      refute_received {:terminal, _}
    end
  end

  test "both SDKs annotate the args and post-process the downstream result" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})

      result = :tenon.call(ctx, :"tools/execute", [%{"cmd" => "echo hi"}], &terminal/1)

      assert result == %{
               "guarded" => "demo",
               "result" => %{"ran" => "echo hi", "seen" => [%{"by" => "demo"}]}
             }
    end
  end

  test "both SDKs count emit-mode events" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})
      assert :tenon.svc(ctx, :demo, :count, []) == 0

      Enum.each(1..3, fn n -> :tenon.emit(ctx, :"sys/audit", [%{"n" => n}]) end)

      assert wait_until(fn -> :tenon.svc(ctx, :demo, :count, []) == 3 end)
    end
  end

  test "a python hook calls into the typescript plugin while typescript serves the hook" do
    ctx = kernel()
    mount(ctx, :py, %{"service" => "demo_py", "peer" => "demo_ts"})
    mount(ctx, :ts, %{"service" => "demo_ts", "peer" => "demo_py"})

    result = :tenon.call(ctx, :"tools/execute", [%{"cmd" => "echo hi"}], &terminal/1)

    assert %{"guarded" => "demo_ts", "result" => %{"guarded" => "demo_py", "result" => inner}} =
             result

    assert inner == %{
             "ran" => "echo hi",
             "seen" => [
               %{"by" => "demo_ts", "peer" => "pong"},
               %{"by" => "demo_py", "peer" => "pong"}
             ]
           }
  end

  test "a typescript hook calls into the python plugin while python serves the hook" do
    ctx = kernel()
    mount(ctx, :ts, %{"service" => "demo_ts", "peer" => "demo_py"})
    mount(ctx, :py, %{"service" => "demo_py", "peer" => "demo_ts"})

    result = :tenon.call(ctx, :"tools/execute", [%{"cmd" => "echo hi"}], &terminal/1)

    assert %{"guarded" => "demo_py", "result" => %{"guarded" => "demo_ts", "result" => inner}} =
             result

    assert inner == %{
             "ran" => "echo hi",
             "seen" => [
               %{"by" => "demo_py", "peer" => "pong"},
               %{"by" => "demo_ts", "peer" => "pong"}
             ]
           }
  end

  test "unmount ends the OS process of both SDKs" do
    for lang <- @langs do
      ctx = kernel()
      fiber = mount(ctx, lang, %{"service" => "demo"})
      os_pid = :tenon.svc(ctx, :demo, :pid, [])
      assert is_integer(os_pid)
      assert alive?(os_pid)

      assert :tenon.unmount(fiber) == :ok

      assert wait_until(fn -> not alive?(os_pid) end)
    end
  end

  test "both SDKs refuse a reply over the frame cap and stay usable" do
    for lang <- @langs do
      ctx = kernel(%{max_frame: 4096})
      fiber = mount(ctx, lang, %{"service" => "demo"})

      assert :tenon.svc(ctx, :demo, :big, [200_000]) == {:error, "frame_too_large"}

      assert :tenon.status(fiber) == :active
      assert :tenon.svc(ctx, :demo, :add, [1, 2]) == 3
    end
  end

  defp terminal(request), do: %{"ran" => request["cmd"], "seen" => request["seen"]}

  defp mounted(lang, config) do
    ctx = kernel()
    mount(ctx, lang, config)
    ctx
  end

  defp kernel(opts \\ %{max_frame: @cap}) do
    {:ok, k} = :tenon.start(opts)
    on_exit(fn -> shutdown(k) end)
    :tenon.root(k)
  end

  defp shutdown(kernel) do
    if Process.alive?(kernel) do
      kernel |> :tenon.tree() |> Map.get(:children, []) |> Enum.each(&:tenon.unmount(&1.pid))
      :tenon.stop(kernel)
    end
  end

  defp mount(ctx, lang, config) do
    {:ok, fiber} =
      :tenon.mount(ctx, %{
        cmd: String.to_charlist(runtime(lang)),
        args: [String.to_charlist(@scripts[lang])],
        config: config
      })

    assert :tenon.status(fiber) == :active
    fiber
  end

  defp runtime(:py), do: executable(["python3"], [])
  defp runtime(:ts), do: executable(["node"], ["~/.nvm/versions/node/v24.14.0/bin/node"])

  defp executable(names, extra) do
    found =
      Enum.find_value(names, &System.find_executable/1) ||
        Enum.find(extra, &File.exists?(Path.expand(&1)))

    found || raise "none of #{inspect(names ++ extra)} found on this machine"
  end

  defp build_typescript do
    dir = Path.join(@sdk, "ts")
    pnpm = executable(["pnpm"], ["~/.local/share/pnpm/pnpm"])
    unless File.dir?(Path.join(dir, "node_modules")), do: run!(pnpm, ["install"], dir)
    run!(pnpm, ["exec", "tsc", "-p", "."], dir)
  end

  defp run!(exe, args, dir) do
    path = Path.dirname(runtime(:ts)) <> ":" <> System.get_env("PATH", "")

    case System.cmd(exe, args, cd: dir, env: [{"PATH", path}], stderr_to_stdout: true) do
      {_out, 0} -> :ok
      {out, code} -> raise "#{exe} #{Enum.join(args, " ")} failed with #{code}\n#{out}"
    end
  end

  defp alive?(os_pid) do
    match?({_, 0}, System.cmd("kill", ["-0", to_string(os_pid)], stderr_to_stdout: true))
  end

  defp wait_until(fun, timeout \\ 5_000) do
    deadline = System.monotonic_time(:millisecond) + timeout
    poll(fun, deadline)
  end

  defp poll(fun, deadline) do
    cond do
      fun.() -> true
      System.monotonic_time(:millisecond) > deadline -> false
      true -> :timer.sleep(20) && poll(fun, deadline)
    end
  end
end
