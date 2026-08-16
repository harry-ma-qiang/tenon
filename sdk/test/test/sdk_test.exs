defmodule SdkTest do
  use ExUnit.Case, async: false

  @sdk Path.expand("../..", __DIR__)
  @scripts %{
    py: Path.join(@sdk, "py/example.py"),
    ts: Path.join(@sdk, "ts/dist/example.js"),
    rs: Path.join(@sdk, "rs/target/release/example")
  }
  @langs [:py, :ts, :rs]
  @term Path.expand("../../../plugins/term", __DIR__)
  @term_bin Path.join(@term, "target/release/tenon-term")
  @cap 65_536

  setup_all do
    build_typescript()
    build_rust(Path.join(@sdk, "rs"))
    build_rust(@term)
    :ok
  end

  test "every SDK answers the same service methods" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})

      assert :tenon.svc(ctx, :demo, :ping, []) == "pong"
      assert :tenon.svc(ctx, :demo, :add, [2, 3]) == 5
      assert :tenon.svc(ctx, :demo, :getenv, ["TENON_MAX_FRAME"]) == "65536"
      assert :tenon.svc(ctx, :demo, :getenv, ["TENON_KERNEL_DEADLINE"]) == "30000"
      assert :tenon.svc(ctx, :demo, :unknown, []) == {:error, "unknown method unknown"}
    end
  end

  test "every SDK blocks a dangerous command without running the terminal" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})
      me = self()
      terminal = fn request -> send(me, {:terminal, request}) end

      result = :tenon.call(ctx, :"tools/execute", [%{"cmd" => "rm -rf /"}], terminal)

      assert result == %{"status" => "blocked", "by" => "demo", "cmd" => "rm -rf /"}
      refute_received {:terminal, _}
    end
  end

  test "every SDK annotates the args and post-processes the downstream result" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})

      result = :tenon.call(ctx, :"tools/execute", [%{"cmd" => "echo hi"}], &terminal/1)

      assert result == %{
               "guarded" => "demo",
               "result" => %{"ran" => "echo hi", "seen" => [%{"by" => "demo"}]}
             }
    end
  end

  test "every SDK counts emit-mode events" do
    for lang <- @langs do
      ctx = mounted(lang, %{"service" => "demo"})
      assert :tenon.svc(ctx, :demo, :count, []) == 0

      Enum.each(1..3, fn n -> :tenon.emit(ctx, :"sys/audit", [%{"n" => n}]) end)

      assert wait_until(fn -> :tenon.svc(ctx, :demo, :count, []) == 3 end)
    end
  end

  test "a python hook calls into the typescript plugin while typescript serves the hook" do
    assert nested(:py, :ts) == ["pong", "pong"]
  end

  test "a typescript hook calls into the python plugin while python serves the hook" do
    assert nested(:ts, :py) == ["pong", "pong"]
  end

  test "a rust hook calls into the python plugin while python serves the hook" do
    assert nested(:rs, :py) == ["pong", "pong"]
  end

  test "a python hook calls into the rust plugin while rust serves the hook" do
    assert nested(:py, :rs) == ["pong", "pong"]
  end

  test "unmount ends the OS process of every SDK" do
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

  test "every SDK refuses a reply over the frame cap and stays usable" do
    for lang <- @langs do
      ctx = kernel(%{max_frame: 4096})
      fiber = mount(ctx, lang, %{"service" => "demo"})

      assert :tenon.svc(ctx, :demo, :big, [200_000]) == {:error, :frame_too_large}

      assert :tenon.status(fiber) == :active
      assert :tenon.svc(ctx, :demo, :add, [1, 2]) == 3
    end
  end

  test "term exec runs a command and inlines small output" do
    ctx = mounted_term()

    result = :tenon.svc(ctx, :term, :exec, ["echo", ["hi"]])

    assert result == %{"status" => 0, "stdout" => "hi\n", "stderr" => "", "truncated" => false}
    assert %{"status" => 2} = :tenon.svc(ctx, :term, :exec, ["ls", ["/no/such/path"]])
  end

  test "term exec honours cwd and kills a command that outruns its timeout" do
    ctx = mounted_term()

    assert %{"stdout" => "/tmp\n"} = :tenon.svc(ctx, :term, :exec, ["pwd", [], "/tmp"])
    assert %{"status" => -1} = :tenon.svc(ctx, :term, :exec, ["sleep", ["5"], "", 200])
  end

  test "term exec spills large output to a handle that read pages back" do
    ctx = mounted_term()

    result = :tenon.svc(ctx, :term, :exec, ["sh", ["-c", "yes x | head -c 200000"]])

    assert %{"truncated" => true, "stderr" => "", "status" => 0} = result
    assert %{"stdout" => %{"handle" => handle, "bytes" => 200_000}} = result
    assert File.exists?(handle)
    assert :tenon.svc(ctx, :term, :read, [handle, 0, 10]) == "x\nx\nx\nx\nx\n"
    assert :tenon.svc(ctx, :term, :read, [handle, 199_996, 10]) == "x\nx\n"
  end

  test "term kill ends a spawned process and emits term/exit" do
    ctx = mounted_term()
    watch(ctx)

    assert %{"pid" => pid, "log" => log} = :tenon.svc(ctx, :term, :spawn, ["sleep", ["30"]])
    assert alive?(pid)
    assert File.exists?(log)

    assert %{"status" => 137} = :tenon.svc(ctx, :term, :kill, [pid])

    assert_receive {:term_exit, %{"pid" => ^pid, "status" => 137}}, 5_000
    assert wait_until(fn -> not alive?(pid) end)
    assert :tenon.svc(ctx, :term, :kill, [pid]) == {:error, "unknown pid #{pid}"}
  end

  test "term reports a natural exit on the next request it serves" do
    ctx = mounted_term()
    watch(ctx)

    assert %{"pid" => pid} = :tenon.svc(ctx, :term, :spawn, ["sh", ["-c", "sleep 0.2; exit 7"]])

    assert wait_until(fn ->
             "pong" = :tenon.svc(ctx, :term, :ping, [])
             mailbox?()
           end)

    assert_receive {:term_exit, %{"pid" => ^pid, "status" => 7}}, 5_000
  end

  test "unmounting term kills the processes it spawned" do
    ctx = kernel()
    fiber = mount_term(ctx)

    assert %{"pid" => pid} = :tenon.svc(ctx, :term, :spawn, ["sleep", ["30"]])
    assert alive?(pid)

    assert :tenon.unmount(fiber) == :ok

    assert wait_until(fn -> not alive?(pid) end)
  end

  defp nested(first, second) do
    ctx = kernel()
    outer = "demo_#{first}"
    inner = "demo_#{second}"
    mount(ctx, first, %{"service" => outer, "peer" => inner})
    mount(ctx, second, %{"service" => inner, "peer" => outer})

    result = :tenon.call(ctx, :"tools/execute", [%{"cmd" => "echo hi"}], &terminal/1)

    assert %{"guarded" => ^inner, "result" => %{"guarded" => ^outer, "result" => body}} = result

    assert %{
             "ran" => "echo hi",
             "seen" => [%{"by" => ^inner, "peer" => one}, %{"by" => ^outer, "peer" => two}]
           } = body

    [one, two]
  end

  defp mailbox?, do: match?({:messages, [_ | _]}, Process.info(self(), :messages))

  defp mounted_term do
    ctx = kernel()
    mount_term(ctx)
    ctx
  end

  defp mount_term(ctx) do
    {:ok, fiber} =
      :tenon.mount(ctx, %{
        cmd: String.to_charlist(@term_bin),
        args: [],
        config: %{"service" => "term"}
      })

    assert :tenon.status(fiber) == :active
    fiber
  end

  defp watch(ctx) do
    me = self()
    :tenon.on(ctx, :"term/exit", fn info -> send(me, {:term_exit, info}) end)
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
    {exe, args} = command(lang)

    {:ok, fiber} =
      :tenon.mount(ctx, %{
        cmd: String.to_charlist(exe),
        args: Enum.map(args, &String.to_charlist/1),
        config: config
      })

    assert :tenon.status(fiber) == :active
    fiber
  end

  defp command(:py), do: {executable(["python3"], []), [@scripts.py]}
  defp command(:ts), do: {node_exe(), [@scripts.ts]}
  defp command(:rs), do: {@scripts.rs, []}

  defp node_exe, do: executable(["node"], ["~/.nvm/versions/node/v24.14.0/bin/node"])

  defp cargo, do: executable(["cargo"], ["~/.cargo/bin/cargo"])

  defp executable(names, extra) do
    found =
      Enum.find_value(names, &System.find_executable/1) ||
        Enum.find_value(extra, fn path ->
          expanded = Path.expand(path)
          if File.exists?(expanded), do: expanded
        end)

    found || raise "none of #{inspect(names ++ extra)} found on this machine"
  end

  defp build_typescript do
    dir = Path.join(@sdk, "ts")
    pnpm = executable(["pnpm"], ["~/.local/share/pnpm/pnpm"])

    unless File.dir?(Path.join(dir, "node_modules")),
      do: run!(pnpm, ["install"], dir, node_path())

    run!(pnpm, ["exec", "tsc", "-p", "."], dir, node_path())
  end

  defp build_rust(dir) do
    run!(cargo(), ["build", "--release"], dir, Path.dirname(cargo()))
  end

  defp node_path, do: Path.dirname(node_exe())

  defp run!(exe, args, dir, prefix) do
    path = prefix <> ":" <> System.get_env("PATH", "")

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
