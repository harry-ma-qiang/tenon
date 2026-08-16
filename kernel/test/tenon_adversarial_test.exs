defmodule Tenon.Adversarial.HotSwapHook do
  def load(ctx, %{pid: pid, tag: tag}) do
    :tenon.on(ctx, :adv_hotswap_ping, fn -> send(pid, {:hotswap_ping, tag}) end)
    :ok
  end
end

defmodule Tenon.Adversarial.NeedsHotDb do
  def inject, do: [:hotdb_v2]

  def load(_ctx, %{pid: pid}) do
    send(pid, :neededhotdb_loaded)
    :ok
  end
end

defmodule Tenon.Adversarial.Provider do
  def load(ctx, %{name: name, impl: impl}) do
    :tenon.provide(ctx, name, impl)
    :ok
  end
end

defmodule Tenon.Adversarial.DepOne do
  def inject, do: [:adv_svc_1]

  def load(_ctx, %{pid: pid}) do
    send(pid, :dep_one_loaded)
    :ok
  end
end

defmodule Tenon.Adversarial.DepNoise do
  def inject, do: [:adv_svc_2]

  def load(_ctx, _config), do: :ok
end

defmodule Tenon.Adversarial.HookSvc do
  def load(ctx, %{name: name}) do
    :tenon.on(ctx, :adv_conc_event, fn -> :ok end)
    :tenon.provide(ctx, name, :adv_impl)
    :ok
  end
end

defmodule Tenon.Adversarial.InlineSvc do
  def load(ctx, _config) do
    :tenon.provide(ctx, :isvc, __MODULE__)
    :ok
  end

  def echo(value), do: value
end

defmodule Tenon.Adversarial.NeedsWireSvc do
  def inject, do: [:advsvc]

  def load(ctx, %{pid: pid}) do
    send(pid, {:wireservice_loaded, :tenon.get(ctx, :advsvc)})
    :tenon.effect(ctx, fn -> fn -> send(pid, :wireservice_unloaded) end end)
    :ok
  end
end

defmodule Tenon.Adversarial.WireParent do
  def load(ctx, %{pid: pid, plugin: plugin}) do
    {:ok, child} =
      :tenon.mount(ctx, %{
        cmd: String.to_charlist(plugin),
        args: [String.to_charlist("misc")],
        config: %{}
      })

    send(pid, {:child, child})
    :ok
  end
end

defmodule Tenon.Adversarial.Reconfig do
  def load(ctx, %{pid: pid, tag: tag}) do
    :tenon.on(ctx, :adv_reconfig_ping, fn -> send(pid, {:ping, tag}) end)
    send(pid, {:loaded, tag})
    :ok
  end
end

defmodule Tenon.Adversarial.SelfStatus do
  def load(ctx, %{pid: pid}) do
    :tenon.on(ctx, :adv_self_status, fn ->
      send(pid, {:self_status_result, :tenon.status(ctx.fiber)})
    end)

    :tenon.emit(ctx, :adv_self_status, [])
    send(pid, :emit_returned)
    :ok
  end
end

defmodule TenonAdversarialTest do
  use ExUnit.Case, async: false

  @plugin Path.join(System.tmp_dir!(), "tenon_adversarial_plugin.py")

  @source ~S"""
  #!/usr/bin/env python3
  import sys, struct, json, os, time

  MODE = sys.argv[1] if len(sys.argv) > 1 else "misc"


  WIRE_IN = os.fdopen(3, "rb", 0)
  WIRE_OUT = os.fdopen(4, "wb", 0)


  def readn(size):
      buf = b""
      while len(buf) < size:
          chunk = WIRE_IN.read(size - len(buf))
          if not chunk:
              return None
          buf += chunk
      return buf


  def recv():
      head = readn(4)
      if head is None:
          return None
      body = readn(struct.unpack(">I", head)[0])
      if body is None:
          return None
      return json.loads(body.decode())


  def send(frame):
      send_raw(json.dumps(frame).encode())


  def send_raw(payload):
      WIRE_OUT.write(struct.pack(">I", len(payload)) + payload)


  if MODE == "silent":
      time.sleep(30)
      sys.exit(0)

  send({"t": "hello", "inject": []})

  next_id = [1000]


  def new_id():
      next_id[0] += 1
      return next_id[0]


  def wait_rep(want_req):
      while True:
          frame = recv()
          if frame is None:
              return None
          if frame.get("t") == "rep" and frame.get("id") == want_req:
              return frame


  while True:
      frame = recv()
      if frame is None:
          break
      kind = frame.get("t")
      if kind == "load":
          send({"t": "on", "hook": 1, "event": "adv/malformed", "arity": 0, "mode": "emit"})
          send({"t": "on", "hook": 2, "event": "adv/no_t", "arity": 0, "mode": "emit"})
          send({"t": "on", "hook": 3, "event": "adv/flood", "arity": 0, "mode": "emit"})
          send({"t": "on", "hook": 4, "event": "adv/badnext", "arity": 1, "mode": "call"})
          send({"t": "on", "hook": 5, "event": "adv/never_answer", "arity": 1, "mode": "call"})
          send({"t": "on", "hook": 6, "event": "adv/cross_call", "arity": 1, "mode": "call"})
          send({"t": "provide", "name": "advsvc"})
          send({"t": "rep", "req": frame["req"], "result": "ok"})
      elif kind == "unload":
          if MODE == "stubborn":
              continue
          sys.exit(0)
      elif kind == "hook":
          event = frame.get("event")
          mode = frame.get("mode")
          args = frame.get("args", [])
          if event == "adv/malformed":
              send_raw(b"not-json-at-all{{{")
          elif event == "adv/no_t":
              send({"foo": "bar"})
          elif event == "adv/flood":
              for i in range(20000):
                  send({"t": "emit", "event": "adv/flooded", "args": [i]})
          elif event == "adv/badnext":
              send({"t": "next", "req": frame["req"], "args": [args[0], args[0]], "await": False})
          elif event == "adv/never_answer":
              pass
          elif event == "adv/cross_call":
              req_id = new_id()
              send({"t": "svc", "id": req_id, "name": "isvc", "method": "echo", "args": [args[0]]})
              rep = wait_rep(req_id)
              result = rep.get("result") if rep else None
              send({"t": "next", "req": frame["req"], "args": [result], "await": False})
          elif mode == "call":
              send({"t": "next", "req": frame["req"], "args": args, "await": False})
      elif kind == "result":
          send({"t": "rep", "req": frame["req"], "result": frame.get("result")})
      elif kind == "svc":
          method = frame.get("method")
          args = frame.get("args", [])
          if method == "add":
              send({"t": "rep", "req": frame["req"], "result": args[0] + args[1]})
          elif method == "echo":
              send({"t": "rep", "req": frame["req"], "result": args[0]})
          elif method == "ospid":
              send({"t": "rep", "req": frame["req"], "result": os.getpid()})
          elif method == "ghost":
              send({"t": "rep", "req": 999999999, "result": "nobody-asked"})
              send({"t": "rep", "req": frame["req"], "result": "real"})
          else:
              send({"t": "rep", "req": frame["req"], "error": "unknown"})
  sys.exit(0)
  """

  setup_all do
    File.write!(@plugin, @source)
    File.chmod!(@plugin, 0o755)
    :ok
  end

  defp kernel(opts \\ %{}) do
    {:ok, k} = :tenon.start_link(opts)
    {k, :tenon.root(k)}
  end

  defp wire_plugin(ctx, mode, config \\ %{}) do
    :tenon.mount(ctx, %{
      cmd: String.to_charlist(@plugin),
      args: [String.to_charlist(mode)],
      config: config
    })
  end

  defp collected(acc \\ []) do
    receive do
      {:hook, value} -> collected([value | acc])
    after
      0 -> Enum.reverse(acc)
    end
  end

  defp wait_until(fun, timeout \\ 3_000) do
    deadline = System.monotonic_time(:millisecond) + timeout
    poll(fun, deadline)
  end

  defp poll(fun, deadline) do
    cond do
      fun.() ->
        :ok

      System.monotonic_time(:millisecond) > deadline ->
        flunk("wait_until timed out")

      true ->
        Process.sleep(5)
        poll(fun, deadline)
    end
  end

  test "hot swap keeps the kernel and live fibers running across two back-to-back reloads" do
    {k, ctx} = kernel()

    {:ok, echo1} =
      :tenon.mount(ctx, %{module: Tenon.Adversarial.HotSwapHook, config: %{pid: self(), tag: :h1}})

    {:ok, wire_fiber} = wire_plugin(ctx, "misc")
    assert :tenon.status(wire_fiber) == :active

    {:ok, pending_dep} =
      :tenon.mount(ctx, %{module: Tenon.Adversarial.NeedsHotDb, config: %{pid: self()}})

    assert :tenon.status(pending_dep) == :pending

    live_pids = [k, ctx.fiber, echo1, wire_fiber, pending_dep]

    reload = fn ->
      :code.purge(:tenon)
      assert {:module, :tenon} = :code.load_file(:tenon)
    end

    reload.()

    assert Enum.all?(live_pids, &Process.alive?/1)
    assert %{module: :undefined} = :tenon.tree(k)
    :tenon.emit(ctx, :adv_hotswap_ping, [])
    assert_receive {:hotswap_ping, :h1}
    assert :tenon.svc(ctx, :advsvc, :add, [2, 3]) == 5

    {:ok, fresh1} =
      :tenon.mount(ctx, %{
        module: Tenon.Adversarial.Provider,
        config: %{name: :hs_after_1, impl: 1}
      })

    assert :tenon.status(fresh1) == :active
    assert :tenon.unmount(fresh1) == :ok

    reload.()

    assert Enum.all?(live_pids, &Process.alive?/1)
    :tenon.emit(ctx, :adv_hotswap_ping, [])
    assert_receive {:hotswap_ping, :h1}
    assert :tenon.svc(ctx, :advsvc, :add, [4, 5]) == 9

    {:ok, fresh2} =
      :tenon.mount(ctx, %{
        module: Tenon.Adversarial.Provider,
        config: %{name: :hs_after_2, impl: 2}
      })

    assert :tenon.status(fresh2) == :active
    assert :tenon.unmount(fresh2) == :ok

    before_unmount_hooks = :ets.info(ctx.tabs.hooks, :size)
    assert :tenon.unmount(echo1) == :ok
    after_unmount_hooks = :ets.info(ctx.tabs.hooks, :size)
    assert after_unmount_hooks < before_unmount_hooks

    {:ok, _hotdb} =
      :tenon.mount(ctx, %{
        module: Tenon.Adversarial.Provider,
        config: %{name: :hotdb_v2, impl: :late}
      })

    assert :tenon.status(pending_dep) == :active
    assert_receive :neededhotdb_loaded
  end

  test "dispatch cost tracks the number of matching hooks, not the size of the hooks table" do
    {_k, ctx} = kernel()

    Enum.each(1..2000, fn e ->
      Enum.each(1..5, fn _ -> :tenon.on(ctx, :"adv_noise_#{e}", fn _ -> :ok end) end)
    end)

    Enum.each(1..3, fn _ -> :tenon.on(ctx, :adv_target, fn _ -> :ok end) end)
    assert :ets.info(ctx.tabs.hooks, :size) == 10_003

    {us_loaded, :ok} =
      :timer.tc(fn -> Enum.each(1..100_000, fn i -> :tenon.emit(ctx, :adv_target, [i]) end) end)

    {_k2, ctx2} = kernel()
    Enum.each(1..3, fn _ -> :tenon.on(ctx2, :adv_target, fn _ -> :ok end) end)

    {us_empty, :ok} =
      :timer.tc(fn -> Enum.each(1..100_000, fn i -> :tenon.emit(ctx2, :adv_target, [i]) end) end)

    ms_loaded = div(us_loaded, 1000)
    ms_empty = div(us_empty, 1000)

    IO.puts(
      "\nadversarial perf: 100k emits, 10_003-row hooks table = #{ms_loaded} ms; 3-row table = #{ms_empty} ms"
    )

    assert us_loaded < us_empty * 3
  end

  test "provide/unprovide churn cost scales with total fiber count, not with relevance" do
    {_k, ctx} = kernel()

    Enum.each(2..5000, fn i ->
      {:ok, _pid} =
        :tenon.mount(ctx, %{
          module: Tenon.Adversarial.Provider,
          config: %{name: :"adv_svc_#{i}", impl: i}
        })
    end)

    {:ok, provider_one} =
      :tenon.mount(ctx, %{
        module: Tenon.Adversarial.Provider,
        config: %{name: :adv_svc_1, impl: 1}
      })

    {:ok, dep_one} =
      :tenon.mount(ctx, %{module: Tenon.Adversarial.DepOne, config: %{pid: self()}})

    assert_receive :dep_one_loaded
    assert :tenon.status(dep_one) == :active

    Enum.each(1..4999, fn _ ->
      {:ok, _pid} = :tenon.mount(ctx, %{module: Tenon.Adversarial.DepNoise, config: %{}})
    end)

    total = :ets.info(ctx.tabs.fibers, :size)
    assert total >= 10_001

    {us, _} =
      :timer.tc(fn ->
        Enum.reduce(1..100, provider_one, fn _, current ->
          :ok = :tenon.unmount(current)
          assert :tenon.status(dep_one) == :pending

          {:ok, next} =
            :tenon.mount(ctx, %{
              module: Tenon.Adversarial.Provider,
              config: %{name: :adv_svc_1, impl: 1}
            })

          assert :tenon.status(dep_one) == :active
          next
        end)
      end)

    ms = div(us, 1000)

    IO.puts(
      "adversarial perf: 100 provide/unprovide cycles of one service over #{total} total fibers = #{ms} ms " <>
        "(notify does a full ets:tab2list scan of the fibers table on every provide/unprovide)"
    )

    assert ms < 5_000
  end

  test "50 concurrent mounters churning 20 fibers each leave no residue, repeated 3 times" do
    {k, ctx} = kernel()
    base_fibers = :ets.info(ctx.tabs.fibers, :size)
    base_hooks = :ets.info(ctx.tabs.hooks, :size)
    base_services = :ets.info(ctx.tabs.services, :size)

    Enum.each(1..3, fn round ->
      1..50
      |> Enum.map(fn w ->
        Task.async(fn ->
          Enum.each(1..20, fn i ->
            {:ok, pid} =
              :tenon.mount(ctx, %{
                module: Tenon.Adversarial.HookSvc,
                config: %{name: :"adv_conc_#{round}_#{w}_#{i}"}
              })

            :tenon.unmount(pid)
          end)
        end)
      end)
      |> Task.await_many(60_000)

      assert :ets.info(ctx.tabs.fibers, :size) == base_fibers
      assert :ets.info(ctx.tabs.hooks, :size) == base_hooks
      assert :ets.info(ctx.tabs.services, :size) == base_services
      assert %{children: []} = :tenon.tree(k)
    end)
  end

  test "a plugin that never sends hello fails after the deadline, not before" do
    {_k, ctx} = kernel(%{deadline: 300})
    started = System.monotonic_time(:millisecond)
    {:ok, fiber} = wire_plugin(ctx, "silent")
    elapsed = System.monotonic_time(:millisecond) - started

    assert :tenon.status(fiber) == :failed
    assert elapsed >= 250
    assert Process.alive?(fiber)
  end

  @tag :capture_log
  test "polling status on a deadline-failed external plugin does not silently respawn it" do
    {_k, ctx} = kernel(%{deadline: 300})

    log =
      ExUnit.CaptureLog.capture_log(fn ->
        {:ok, fiber} = wire_plugin(ctx, "silent")
        assert :tenon.status(fiber) == :failed
        Process.sleep(400)
        assert :tenon.status(fiber) == :failed
        Process.sleep(400)
        assert :tenon.status(fiber) == :failed
        send(self(), :done)
      end)

    assert_received :done

    load_failures =
      log
      |> String.split("\n")
      |> Enum.count(&String.contains?(&1, "failed to load: :timeout"))

    assert load_failures == 1,
           "expected exactly one load attempt for a fiber with no injects, got #{load_failures}: read-only status/1 polling should not respawn a failed external plugin"
  end

  @tag :capture_log
  test "a malformed wire frame does not take down the kernel" do
    {_k, ctx} = kernel()
    {:ok, fiber} = wire_plugin(ctx, "misc")
    assert :tenon.status(fiber) == :active

    :tenon.emit(ctx, :"adv/no_t", [])
    Process.sleep(200)
    assert Process.alive?(fiber)
    assert :tenon.svc(ctx, :advsvc, :add, [1, 2]) == 3

    :tenon.emit(ctx, :"adv/malformed", [])
    Process.sleep(200)

    assert Process.alive?(fiber)
    assert :tenon.svc(ctx, :advsvc, :add, [4, 5]) == 9
  end

  test "unmount takes about grace when the plugin ignores unload, then kills the OS process" do
    {_k, ctx} = kernel(%{grace: 300})
    {:ok, fiber} = wire_plugin(ctx, "stubborn")
    assert :tenon.status(fiber) == :active

    os_pid = :tenon.svc(ctx, :advsvc, :ospid, [])
    assert File.exists?("/proc/#{os_pid}")

    {us, :ok} = :timer.tc(fn -> :tenon.unmount(fiber) end)
    ms = div(us, 1000)

    IO.puts("adversarial wire: unmount of a stubborn plugin took #{ms} ms (grace = 300 ms)")

    assert ms >= 250
    assert ms < 3_000
    wait_until(fn -> not File.exists?("/proc/#{os_pid}") end)
  end

  test "a plugin flooding 20k emits survives and every hook fires" do
    {_k, ctx} = kernel()
    counter = :counters.new(1, [])
    :tenon.on(ctx, :"adv/flooded", fn _v -> :counters.add(counter, 1, 1) end)

    {:ok, fiber} = wire_plugin(ctx, "misc")
    assert :tenon.status(fiber) == :active

    :tenon.emit(ctx, :"adv/flood", [])
    wait_until(fn -> :counters.get(counter, 1) == 20_000 end, 15_000)

    :erlang.garbage_collect(fiber)
    {:memory, mem} = Process.info(fiber, :memory)
    IO.puts("adversarial wire: fiber memory after 20k emit flood + gc = #{mem} bytes")

    assert mem < 2_000_000
    assert Process.alive?(fiber)
    assert :tenon.svc(ctx, :advsvc, :add, [1, 2]) == 3
  end

  test "a plugin replying next with the wrong arity errors the caller, not the fiber" do
    {_k, ctx} = kernel()
    {:ok, fiber} = wire_plugin(ctx, "misc")
    assert :tenon.status(fiber) == :active

    assert_raise ErlangError, fn ->
      :tenon.call(ctx, :"adv/badnext", [1], fn v -> v end)
    end

    assert Process.alive?(fiber)
    assert :tenon.svc(ctx, :advsvc, :add, [1, 2]) == 3
  end

  test "a reply to an unknown request id is ignored" do
    {_k, ctx} = kernel()
    {:ok, fiber} = wire_plugin(ctx, "misc")

    assert :tenon.svc(ctx, :advsvc, :ghost, []) == "real"
    assert Process.alive?(fiber)
    assert :tenon.svc(ctx, :advsvc, :add, [1, 2]) == 3
  end

  test "a call-mode hook that never answers times out, and later frames still work" do
    {_k, ctx} = kernel(%{deadline: 300})
    {:ok, fiber} = wire_plugin(ctx, "misc")

    {us, result} = :timer.tc(fn -> :tenon.call(ctx, :"adv/never_answer", [1], fn v -> v end) end)
    ms = div(us, 1000)

    IO.puts("adversarial wire: silent call-mode hook timed out in #{ms} ms (deadline = 300 ms)")

    assert result == {:error, :timeout}
    assert ms >= 250
    assert Process.alive?(fiber)
    assert :tenon.svc(ctx, :advsvc, :add, [1, 2]) == 3
  end

  @tag :capture_log
  test "a hook calling status on its own fiber during its own emit does not hang the mount" do
    {_k, ctx} = kernel()
    me = self()

    task =
      Task.async(fn ->
        :tenon.mount(ctx, %{module: Tenon.Adversarial.SelfStatus, config: %{pid: me}})
      end)

    result = Task.yield(task, 2_000) || Task.shutdown(task, :brutal_kill)

    case result do
      {:ok, {:ok, _fiber}} ->
        assert_receive :emit_returned, 500

      other ->
        flunk(
          "mount whose load emits an event whose own hook calls status(self()) did not settle " <>
            "within 2s: #{inspect(other)}"
        )
    end
  end

  test "prepend order survives interleaved removal and re-registration" do
    {_k, ctx} = kernel()
    me = self()

    off_a = :tenon.on(ctx, :"adv/order", fn -> send(me, {:hook, :a}) end)
    off_b = :tenon.on(ctx, :"adv/order", fn -> send(me, {:hook, :b}) end, %{prepend: true})
    _off_c = :tenon.on(ctx, :"adv/order", fn -> send(me, {:hook, :c}) end, %{prepend: true})

    :tenon.emit(ctx, :"adv/order", [])
    assert collected() == [:c, :b, :a]

    off_b.()
    _off_d = :tenon.on(ctx, :"adv/order", fn -> send(me, {:hook, :d}) end, %{prepend: true})

    :tenon.emit(ctx, :"adv/order", [])
    assert collected() == [:d, :c, :a]

    off_a.()
    :tenon.emit(ctx, :"adv/order", [])
    assert collected() == [:d, :c]
  end

  test "a wire plugin can call an inline service, and an inline caller can call a wire service" do
    {_k, ctx} = kernel()
    {:ok, _inline} = :tenon.mount(ctx, %{module: Tenon.Adversarial.InlineSvc, config: %{}})
    {:ok, fiber} = wire_plugin(ctx, "misc")
    assert :tenon.status(fiber) == :active

    assert :tenon.svc(ctx, :advsvc, :add, [10, 5]) == 15
    assert :tenon.call(ctx, :"adv/cross_call", [99], fn v -> v end) == 99
  end

  test "unmounting a parent kills its external child's OS process" do
    {_k, ctx} = kernel()

    {:ok, parent} =
      :tenon.mount(ctx, %{
        module: Tenon.Adversarial.WireParent,
        config: %{pid: self(), plugin: @plugin}
      })

    assert_receive {:child, child}
    assert :tenon.status(child) == :active

    os_pid = :tenon.svc(ctx, :advsvc, :ospid, [])
    assert File.exists?("/proc/#{os_pid}")

    assert :tenon.unmount(parent) == :ok
    refute Process.alive?(child)
    wait_until(fn -> not File.exists?("/proc/#{os_pid}") end)
  end

  test "restart/2 tears down the old config's hooks and installs the new ones" do
    {_k, ctx} = kernel()

    {:ok, pid} =
      :tenon.mount(ctx, %{module: Tenon.Adversarial.Reconfig, config: %{pid: self(), tag: :v1}})

    assert_receive {:loaded, :v1}

    :tenon.emit(ctx, :adv_reconfig_ping, [])
    assert_receive {:ping, :v1}

    hooks_before = :ets.info(ctx.tabs.hooks, :size)

    assert :tenon.restart(pid, %{pid: self(), tag: :v2}) == :ok
    assert_receive {:loaded, :v2}

    assert :ets.info(ctx.tabs.hooks, :size) == hooks_before

    :tenon.emit(ctx, :adv_reconfig_ping, [])
    assert_receive {:ping, :v2}
    refute_received {:ping, :v1}
  end

  test "an inject-dependent of a wire-provided service unloads when the plugin's OS process dies" do
    {_k, ctx} = kernel()
    {:ok, wire_fiber} = wire_plugin(ctx, "misc")
    assert :tenon.status(wire_fiber) == :active

    {:ok, dep} =
      :tenon.mount(ctx, %{module: Tenon.Adversarial.NeedsWireSvc, config: %{pid: self()}})

    assert_receive {:wireservice_loaded, {:tenon_wire, ^wire_fiber, :advsvc}}
    assert :tenon.status(dep) == :active

    os_pid = :tenon.svc(ctx, :advsvc, :ospid, [])
    System.cmd("kill", ["-9", to_string(os_pid)])

    assert_receive :wireservice_unloaded, 3_000
    wait_until(fn -> :tenon.status(dep) == :pending end)
    assert :tenon.get(ctx, :advsvc) == :undefined
  end
end
