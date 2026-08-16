defmodule Tenon.Test.Echo do
  def load(ctx, %{pid: pid, tag: tag}) do
    send(pid, {:loaded, tag})
    :tenon.effect(ctx, fn -> fn -> send(pid, {:disposed, tag}) end end)
    :ok
  end
end

defmodule Tenon.Test.Db do
  def load(ctx, %{impl: impl}) do
    :tenon.provide(ctx, :db, impl)
    :ok
  end
end

defmodule Tenon.Test.Consumer do
  def inject, do: [:db]

  def load(ctx, %{pid: pid, tag: tag}) do
    send(pid, {:consumer_loaded, :tenon.get(ctx, :db)})
    :tenon.effect(ctx, fn -> fn -> send(pid, {:consumer_unloaded, tag}) end end)
    :ok
  end
end

defmodule Tenon.Test.Stack do
  def load(ctx, %{pid: pid, tags: tags}) do
    Enum.each(tags, fn tag ->
      :tenon.effect(ctx, fn -> fn -> send(pid, {:hook, tag}) end end)
    end)

    :ok
  end
end

defmodule Tenon.Test.Registrar do
  def load(ctx, %{name: name, impl: impl}) do
    :tenon.on(ctx, :ping, fn -> :pong end)
    :tenon.provide(ctx, name, impl)
    :ok
  end
end

defmodule Tenon.Test.Parent do
  def load(ctx, %{pid: pid}) do
    :tenon.effect(ctx, fn -> fn -> send(pid, {:hook, :before_child}) end end)

    {:ok, child} =
      :tenon.mount(ctx, %{module: Tenon.Test.Child, config: %{pid: pid, tag: :child}})

    :tenon.effect(ctx, fn -> fn -> send(pid, {:hook, :after_child}) end end)
    send(pid, {:child, child})
    :ok
  end
end

defmodule Tenon.Test.Child do
  def load(ctx, %{pid: pid, tag: tag}) do
    :tenon.effect(ctx, fn -> fn -> send(pid, {:hook, tag}) end end)
    :ok
  end
end

defmodule Tenon.Test.Boom do
  def load(ctx, %{pid: pid, agent: agent}) do
    :tenon.effect(ctx, fn -> fn -> send(pid, {:hook, :partial}) end end)
    if Agent.get(agent, & &1), do: raise("boom")
    send(pid, {:loaded, :boom})
    :ok
  end
end

defmodule Tenon.Test.Tagged do
  def load(_ctx, %{pid: pid, tag: tag}) do
    send(pid, {:tagged, tag})
    :ok
  end
end

defmodule TenonTest do
  use ExUnit.Case, async: false

  @plugin Path.join(System.tmp_dir!(), "tenon_wire_plugin.py")

  @source ~S"""
  #!/usr/bin/env python3
  import sys, struct, json, os

  MODE = sys.argv[1] if len(sys.argv) > 1 else "basic"

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
      body = json.dumps(frame).encode()
      WIRE_OUT.write(struct.pack(">I", len(body)) + body)


  if MODE == "noisy":
      print("plugin stdout is free for logs", flush=True)
      sys.stderr.write("plugin stderr is free for logs\n")
      sys.stderr.flush()

  send({"t": "hello", "inject": ["db"] if MODE == "inject" else []})

  while True:
      frame = recv()
      if frame is None:
          break
      kind = frame.get("t")
      if MODE == "noisy":
          print("plugin saw a %s frame" % kind, flush=True)
          sys.stderr.write("plugin saw a %s frame\n" % kind)
          sys.stderr.flush()
      if kind == "load":
          if MODE == "crash":
              sys.exit(3)
          send({"t": "on", "hook": 1, "event": "wire/emit", "arity": 1, "mode": "emit"})
          send({"t": "on", "hook": 2, "event": "wire/call", "arity": 1, "mode": "call"})
          send({"t": "provide", "name": "pysvc"})
          send({"t": "rep", "req": frame["req"], "result": "ok"})
      elif kind == "unload":
          sys.exit(0)
      elif kind == "hook":
          if frame.get("mode") == "emit":
              send({"t": "emit", "event": "wire/seen", "args": frame.get("args", [])})
          else:
              args = frame.get("args", [])
              send({"t": "next", "req": frame["req"], "args": [args[0] + 1], "await": True})
      elif kind == "result":
          send({"t": "rep", "req": frame["req"], "result": frame.get("result") + 1})
      elif kind == "svc":
          method = frame.get("method")
          args = frame.get("args", [])
          if method == "add":
              send({"t": "rep", "req": frame["req"], "result": args[0] + args[1]})
          elif method == "echo":
              send({"t": "rep", "req": frame["req"], "result": args[0]})
          elif method == "ospid":
              send({"t": "rep", "req": frame["req"], "result": os.getpid()})
          elif method == "big":
              send({"t": "rep", "req": frame["req"], "result": "x" * args[0]})
          elif method == "getenv":
              send({"t": "rep", "req": frame["req"], "result": os.environ.get(args[0], "")})
          elif method == "boom":
              send({"t": "rep", "req": frame["req"], "error": "nope"})
          elif method == "unhook":
              send({"t": "off", "hook": 1})
              send({"t": "unprovide", "name": "pysvc"})
              send({"t": "rep", "req": frame["req"], "result": "gone"})
          elif method == "slow":
              pass
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

  defp wire(ctx, mode, config \\ %{}) do
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

  defp rows(table, pattern), do: :ets.match_object(table, pattern)

  defp epoch(ctx, fiber) do
    [row] = :ets.lookup(ctx.tabs.fibers, fiber)
    elem(row, 7)
  end

  test "a plugin loads on mount and unloads on unmount" do
    {k, ctx} = kernel()
    {:ok, pid} = :tenon.mount(ctx, %{module: Tenon.Test.Echo, config: %{pid: self(), tag: :a}})

    assert_receive {:loaded, :a}
    assert :tenon.status(pid) == :active
    assert %{module: :undefined, status: :active, children: [%{pid: ^pid}]} = :tenon.tree(k)

    assert :tenon.unmount(pid) == :ok
    assert_receive {:disposed, :a}
    refute Process.alive?(pid)
    assert %{children: []} = :tenon.tree(k)
  end

  test "disposers run in reverse registration order" do
    {_k, ctx} = kernel()

    {:ok, pid} =
      :tenon.mount(ctx, %{
        module: Tenon.Test.Stack,
        config: %{pid: self(), tags: [:one, :two, :three]}
      })

    :tenon.unmount(pid)
    assert collected() == [:three, :two, :one]
  end

  test "a single effect disposer removes only its own effect" do
    {_k, ctx} = kernel()
    me = self()
    first = :tenon.effect(ctx, fn -> fn -> send(me, {:hook, :first}) end end)
    _second = :tenon.effect(ctx, fn -> fn -> send(me, {:hook, :second}) end end)

    first.()

    assert collected() == [:first]
  end

  test "an effect body that registers nothing yields an inert disposer" do
    {_k, ctx} = kernel()
    disposer = :tenon.effect(ctx, fn -> :ok end)
    assert disposer.() == :ok
  end

  test "killing a fiber leaves no hook, service or fiber rows" do
    {_k, ctx} = kernel()

    {:ok, pid} =
      :tenon.mount(ctx, %{module: Tenon.Test.Registrar, config: %{name: :thing, impl: :value}})

    assert :tenon.get(ctx, :thing) == :value
    assert :tenon.bail(ctx, :ping, []) == :pong

    Process.exit(pid, :kill)
    wait_until(fn -> :ets.lookup(ctx.tabs.fibers, pid) == [] end)

    assert :tenon.get(ctx, :thing) == :undefined
    assert :tenon.bail(ctx, :ping, []) == :undefined
    assert rows(ctx.tabs.hooks, {{:_, :_}, :_, pid, :_}) == []
    assert rows(ctx.tabs.services, {:_, :_, pid}) == []
  end

  test "a fiber waits for its injected service, then loads" do
    {_k, ctx} = kernel()

    {:ok, consumer} =
      :tenon.mount(ctx, %{module: Tenon.Test.Consumer, config: %{pid: self(), tag: :c}})

    assert :tenon.status(consumer) == :pending
    refute_received {:consumer_loaded, _impl}

    {:ok, _db} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn}})

    assert :tenon.status(consumer) == :active
    assert_receive {:consumer_loaded, :conn}
  end

  test "losing the provider unloads the dependent, regaining it reloads" do
    {_k, ctx} = kernel()
    {:ok, db} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn}})

    {:ok, consumer} =
      :tenon.mount(ctx, %{module: Tenon.Test.Consumer, config: %{pid: self(), tag: :c}})

    assert :tenon.status(consumer) == :active
    assert_receive {:consumer_loaded, :conn}

    :tenon.unmount(db)

    assert :tenon.status(consumer) == :pending
    assert_receive {:consumer_unloaded, :c}
    assert :tenon.get(ctx, :db) == :undefined

    {:ok, _db2} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn2}})

    assert :tenon.status(consumer) == :active
    assert_receive {:consumer_loaded, :conn2}
  end

  test "swapping the provider changes the epoch of the dependent" do
    {_k, ctx} = kernel()
    {:ok, db} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn}})

    {:ok, consumer} =
      :tenon.mount(ctx, %{module: Tenon.Test.Consumer, config: %{pid: self(), tag: :c}})

    assert epoch(ctx, consumer) == [db: db]

    :tenon.unmount(db)
    assert :tenon.status(consumer) == :pending
    assert epoch(ctx, consumer) == :inactive

    {:ok, db2} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn2}})

    assert :tenon.status(consumer) == :active
    assert epoch(ctx, consumer) == [db: db2]
    refute db2 == db
  end

  test "killing the provider unloads the dependent" do
    {_k, ctx} = kernel()
    {:ok, db} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn}})

    {:ok, consumer} =
      :tenon.mount(ctx, %{module: Tenon.Test.Consumer, config: %{pid: self(), tag: :c}})

    assert :tenon.status(consumer) == :active

    Process.exit(db, :kill)
    wait_until(fn -> :tenon.get(ctx, :db) == :undefined end)

    assert :tenon.status(consumer) == :pending
    assert_receive {:consumer_unloaded, :c}
  end

  test "prepend puts a hook in front of the ones already registered" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :ev, fn -> send(me, {:hook, :first}) end)
    :tenon.on(ctx, :ev, fn -> send(me, {:hook, :second}) end)
    :tenon.on(ctx, :ev, fn -> send(me, {:hook, :front}) end, %{prepend: true})
    :tenon.on(ctx, :ev, fn -> send(me, {:hook, :fronter}) end, %{prepend: true})

    :tenon.emit(ctx, :ev, [])

    assert collected() == [:fronter, :front, :first, :second]
  end

  @tag :capture_log
  test "emit runs every hook in registration order and isolates failures" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :ev, fn value -> send(me, {:hook, {:a, value}}) end)
    :tenon.on(ctx, :ev, fn _value -> raise "listener down" end)
    :tenon.on(ctx, :ev, fn value -> send(me, {:hook, {:b, value}}) end)

    assert :tenon.emit(ctx, :ev, [1]) == :ok
    assert collected() == [{:a, 1}, {:b, 1}]
  end

  test "the disposer returned by on removes the hook" do
    {_k, ctx} = kernel()
    me = self()
    off = :tenon.on(ctx, :ev, fn -> send(me, {:hook, :once}) end)

    :tenon.emit(ctx, :ev, [])
    off.()
    :tenon.emit(ctx, :ev, [])

    assert collected() == [:once]
  end

  test "call rewrites arguments and wraps the terminal" do
    {_k, ctx} = kernel()
    :tenon.on(ctx, :ev, fn value, next -> next.(value + 1) end)
    :tenon.on(ctx, :ev, fn value, next -> {:wrapped, next.(value * 2)} end)

    assert :tenon.call(ctx, :ev, [1], fn v -> {:terminal, v} end) == {:wrapped, {:terminal, 4}}
    assert :tenon.call(ctx, :none, [7], fn v -> {:terminal, v} end) == {:terminal, 7}
  end

  test "a call hook that skips next short-circuits the chain" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :ev, fn _value, _next -> :vetoed end)
    :tenon.on(ctx, :ev, fn value, next -> send(me, {:hook, :down}) && next.(value) end)

    assert :tenon.call(ctx, :ev, [1], fn v -> {:terminal, v} end) == :vetoed
    assert collected() == []
  end

  test "call works with zero and five arguments" do
    {_k, ctx} = kernel()
    :tenon.on(ctx, :zero, fn next -> next.() end)
    assert :tenon.call(ctx, :zero, [], fn -> :terminal end) == :terminal

    :tenon.on(ctx, :five, fn a, b, c, d, e, next -> next.(a + 1, b, c, d, e) end)

    assert :tenon.call(ctx, :five, [1, 2, 3, 4, 5], fn a, b, c, d, e -> {a, b, c, d, e} end) ==
             {2, 2, 3, 4, 5}
  end

  test "a raising hook inside call propagates to the caller" do
    {_k, ctx} = kernel()
    :tenon.on(ctx, :ev, fn _value, _next -> raise "call boom" end)

    assert_raise RuntimeError, "call boom", fn ->
      :tenon.call(ctx, :ev, [1], fn v -> v end)
    end
  end

  test "call refuses more arguments than it can wrap" do
    {_k, ctx} = kernel()

    assert_raise ErlangError, fn ->
      :tenon.call(ctx, :ev, [1, 2, 3, 4, 5, 6], fn _a, _b, _c, _d, _e, _f -> :ok end)
    end
  end

  test "bail stops at the first hook returning a value" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :ev, fn _v -> send(me, {:hook, :one}) && :undefined end)
    :tenon.on(ctx, :ev, fn v -> send(me, {:hook, :two}) && {:ok, v * 2} end)
    :tenon.on(ctx, :ev, fn _v -> send(me, {:hook, :three}) end)

    assert :tenon.bail(ctx, :ev, [21]) == {:ok, 42}
    assert collected() == [:one, :two]
    assert :tenon.bail(ctx, :none, []) == :undefined
  end

  test "unmounting a parent disposes its children in reverse effect order" do
    {_k, ctx} = kernel()
    {:ok, parent} = :tenon.mount(ctx, %{module: Tenon.Test.Parent, config: %{pid: self()}})
    assert_receive {:child, child}

    :tenon.unmount(parent)

    assert collected() == [:after_child, :child, :before_child]
    refute Process.alive?(child)
  end

  test "killing a parent disposes its children" do
    {_k, ctx} = kernel()
    {:ok, parent} = :tenon.mount(ctx, %{module: Tenon.Test.Parent, config: %{pid: self()}})
    assert_receive {:child, child}

    Process.exit(parent, :kill)
    wait_until(fn -> not Process.alive?(child) end)

    assert :ets.lookup(ctx.tabs.fibers, child) == []
  end

  @tag :capture_log
  test "a load that raises leaves the fiber failed and restartable" do
    {k, ctx} = kernel()
    {:ok, agent} = Agent.start_link(fn -> true end)

    {:ok, pid} =
      :tenon.mount(ctx, %{module: Tenon.Test.Boom, config: %{pid: self(), agent: agent}})

    assert :tenon.status(pid) == :failed
    assert Process.alive?(pid)
    assert %{children: [%{status: :failed, error: {:error, _reason, _stack}}]} = :tenon.tree(k)
    refute_received {:hook, :partial}

    assert :tenon.restart(pid) == :ok
    assert :tenon.status(pid) == :failed
    assert_receive {:hook, :partial}

    Agent.update(agent, fn _ -> false end)
    assert :tenon.restart(pid) == :ok
    assert :tenon.status(pid) == :active
    assert_receive {:loaded, :boom}
  end

  test "restart with a new config reloads the plugin" do
    {_k, ctx} = kernel()

    {:ok, pid} =
      :tenon.mount(ctx, %{module: Tenon.Test.Tagged, config: %{pid: self(), tag: :old}})

    assert_receive {:tagged, :old}

    assert :tenon.restart(pid, %{pid: self(), tag: :new}) == :ok
    assert_receive {:tagged, :new}
    assert :tenon.status(pid) == :active
  end

  test "fibers announce themselves, their status and their services" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :"internal/plugin", fn fiber -> send(me, {:plugin, fiber}) end)
    :tenon.on(ctx, :"internal/status", fn f, old, new -> send(me, {:status, f, old, new}) end)
    :tenon.on(ctx, :"internal/service", fn name, impl -> send(me, {:service, name, impl}) end)

    {:ok, db} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn}})

    assert_receive {:plugin, ^db}
    assert_receive {:status, ^db, :pending, :loading}
    assert_receive {:status, ^db, :loading, :active}
    assert_receive {:service, :db, :conn}

    :tenon.unmount(db)

    assert_receive {:status, ^db, :active, :unloading}
    assert_receive {:status, ^db, :unloading, :disposed}
    assert_receive {:service, :db, :undefined}
    refute_received {:plugin, ^db}
  end

  test "two kernels share nothing" do
    {_k1, one} = kernel()
    {k2, two} = kernel()

    {:ok, _pid} =
      :tenon.mount(one, %{module: Tenon.Test.Registrar, config: %{name: :thing, impl: :value}})

    :tenon.on(two, :ping, fn -> :other end)

    assert :tenon.get(one, :thing) == :value
    assert :tenon.get(two, :thing) == :undefined
    assert :tenon.bail(one, :ping, []) == :pong
    assert :tenon.bail(two, :ping, []) == :other
    assert one.tabs.hooks != two.tabs.hooks
    assert %{children: []} = :tenon.tree(k2)
  end

  test "providing a name twice raises" do
    {_k, ctx} = kernel()
    :tenon.provide(ctx, :dup, :one)

    assert_raise ErlangError, fn -> :tenon.provide(ctx, :dup, :two) end
  end

  test "unmounting twice is safe" do
    {_k, ctx} = kernel()
    {:ok, pid} = :tenon.mount(ctx, %{module: Tenon.Test.Echo, config: %{pid: self(), tag: :a}})

    assert :tenon.unmount(pid) == :ok
    assert :tenon.unmount(pid) == :ok
  end

  test "mounting and unmounting 500 fibers leaves no residue" do
    {_k, ctx} = kernel()
    fibers = :ets.info(ctx.tabs.fibers, :size)
    hooks = :ets.info(ctx.tabs.hooks, :size)
    services = :ets.info(ctx.tabs.services, :size)

    Enum.each(1..500, fn i ->
      {:ok, pid} =
        :tenon.mount(ctx, %{
          module: Tenon.Test.Registrar,
          config: %{name: :"svc_#{i}", impl: i}
        })

      :tenon.unmount(pid)
    end)

    assert :ets.info(ctx.tabs.fibers, :size) == fibers
    assert :ets.info(ctx.tabs.hooks, :size) == hooks
    assert :ets.info(ctx.tabs.services, :size) == services
    wait_until(fn -> :sys.get_state(ctx.fiber) |> elem(13) == [] end)
  end

  test "an external plugin loads, serves a hook and answers a service" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :"wire/seen", fn value -> send(me, {:seen, value}) end)

    {:ok, fiber} = wire(ctx, "basic")
    assert :tenon.status(fiber) == :active

    :tenon.emit(ctx, :"wire/emit", [42])
    assert_receive {:seen, 42}, 2_000

    assert :tenon.call(ctx, :"wire/call", [1], fn v -> v * 10 end) == 21

    assert {:tenon_wire, ^fiber, :pysvc} = :tenon.get(ctx, :pysvc)
    assert :tenon.svc(ctx, :pysvc, :add, [1, 2]) == 3
    assert :tenon.svc(ctx, :pysvc, :echo, [%{a: 1}]) == %{"a" => 1}
    assert {:error, "nope"} = :tenon.svc(ctx, :pysvc, :boom, [])
  end

  test "an external plugin can withdraw its hook and its service" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :"wire/seen", fn value -> send(me, {:seen, value}) end)

    {:ok, _fiber} = wire(ctx, "basic")

    assert :tenon.svc(ctx, :pysvc, :unhook, []) == "gone"
    wait_until(fn -> :tenon.get(ctx, :pysvc) == :undefined end)

    :tenon.emit(ctx, :"wire/emit", [7])
    refute_receive {:seen, 7}, 200
  end

  test "an external plugin declares inject in hello and waits for the service" do
    {_k, ctx} = kernel()
    {:ok, fiber} = wire(ctx, "inject")

    assert :tenon.status(fiber) == :pending
    assert :ets.lookup(ctx.tabs.fibers, fiber) |> hd() |> elem(6) == [:db]

    {:ok, db} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn}})
    assert :tenon.status(fiber) == :active
    assert :tenon.svc(ctx, :pysvc, :add, [2, 3]) == 5

    :tenon.unmount(db)
    assert :tenon.status(fiber) == :pending

    {:ok, _db2} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :conn2}})
    assert :tenon.status(fiber) == :active
    assert :tenon.svc(ctx, :pysvc, :add, [4, 5]) == 9
  end

  @tag :capture_log
  test "a plugin that exits during load leaves the fiber failed" do
    {k, ctx} = kernel()
    {:ok, fiber} = wire(ctx, "crash")

    assert :tenon.status(fiber) == :failed
    assert %{children: [%{status: :failed, error: {:exit_status, 3}}]} = :tenon.tree(k)
  end

  test "unmount stops the operating system process" do
    {_k, ctx} = kernel()
    {:ok, fiber} = wire(ctx, "basic")

    os_pid = :tenon.svc(ctx, :pysvc, :ospid, [])
    assert is_integer(os_pid)
    assert File.exists?("/proc/#{os_pid}")

    assert :tenon.unmount(fiber) == :ok
    refute Process.alive?(fiber)
    wait_until(fn -> not File.exists?("/proc/#{os_pid}") end)
  end

  test "a wire request that is never answered fails with a timeout" do
    {_k, ctx} = kernel(%{deadline: 600, grace: 600})
    {:ok, fiber} = wire(ctx, "basic")
    assert :tenon.status(fiber) == :active

    assert :tenon.svc(ctx, :pysvc, :slow, []) == {:error, :timeout}
    assert :tenon.svc(ctx, :pysvc, :add, [1, 1]) == 2
  end

  test "a plugin that writes to stdout and stderr still talks over fd 3 and fd 4" do
    {_k, ctx} = kernel()
    me = self()
    :tenon.on(ctx, :"wire/seen", fn value -> send(me, {:seen, value}) end)

    {:ok, fiber} = wire(ctx, "noisy")
    assert :tenon.status(fiber) == :active

    assert :tenon.svc(ctx, :pysvc, :add, [20, 22]) == 42
    :tenon.emit(ctx, :"wire/emit", [7])
    assert_receive {:seen, 7}, 2_000
  end

  @tag :capture_log
  test "a reply over the frame cap is dropped and the caller is told" do
    {_k, ctx} = kernel(%{max_frame: 4096})
    {:ok, fiber} = wire(ctx, "basic")
    assert :tenon.status(fiber) == :active

    assert :tenon.svc(ctx, :pysvc, :big, [100_000]) == {:error, :frame_too_large}

    assert Process.alive?(fiber)
    assert :tenon.status(fiber) == :active
    assert :tenon.svc(ctx, :pysvc, :add, [1, 2]) == 3
  end

  test "the frame cap is configurable by option and by TENON_MAX_FRAME" do
    {_k, ctx} = kernel(%{max_frame: 4_194_304})
    {:ok, fiber} = wire(ctx, "basic")
    assert :tenon.status(fiber) == :active
    assert byte_size(:tenon.svc(ctx, :pysvc, :big, [2_000_000])) == 2_000_000

    previous = System.get_env("TENON_MAX_FRAME")
    System.put_env("TENON_MAX_FRAME", "4194304")

    try do
      {_k2, ctx2} = kernel()
      {:ok, fiber2} = wire(ctx2, "basic")
      assert :tenon.status(fiber2) == :active
      assert byte_size(:tenon.svc(ctx2, :pysvc, :big, [2_000_000])) == 2_000_000
    after
      if previous,
        do: System.put_env("TENON_MAX_FRAME", previous),
        else: System.delete_env("TENON_MAX_FRAME")
    end
  end

  test "a plugin receives the cap and the deadline in its environment" do
    {_k, ctx} = kernel(%{max_frame: 4096, deadline: 7000})
    {:ok, fiber} = wire(ctx, "basic")
    assert :tenon.status(fiber) == :active

    assert :tenon.svc(ctx, :pysvc, :getenv, ["TENON_MAX_FRAME"]) == "4096"
    assert :tenon.svc(ctx, :pysvc, :getenv, ["TENON_KERNEL_DEADLINE"]) == "7000"
  end

  test "perf smoke: 100k emits with three hooks" do
    {_k, ctx} = kernel()
    Enum.each(1..3, fn _ -> :tenon.on(ctx, :perf, fn _v -> :ok end) end)

    {us, :ok} =
      :timer.tc(fn ->
        Enum.each(1..100_000, fn i -> :tenon.emit(ctx, :perf, [i]) end)
      end)

    ms = div(us, 1000)

    IO.puts(
      "\nperf: 100_000 emits x 3 hooks in #{ms} ms (#{div(100_000 * 1000, max(us, 1))} k/s)"
    )

    assert ms < 30_000
  end

  test "perf smoke: 10k wire round trips" do
    {_k, ctx} = kernel()
    {:ok, fiber} = wire(ctx, "basic")
    assert :tenon.status(fiber) == :active

    {us, :ok} =
      :timer.tc(fn ->
        Enum.each(1..10_000, fn i -> ^i = :tenon.svc(ctx, :pysvc, :echo, [i]) end)
      end)

    ms = div(us, 1000)
    IO.puts("perf: 10_000 wire round trips in #{ms} ms (#{div(10_000 * 1000, max(us, 1))} k/s)")
    assert ms < 60_000
  end

  test "status on an unmounted fiber returns disposed" do
    {:ok, k} = :tenon.start_link()
    ctx = :tenon.root(k)
    {:ok, f} = :tenon.mount(ctx, %{module: Tenon.Test.Db, config: %{impl: :x}})
    :ok = :tenon.unmount(f)
    assert :tenon.status(f) == :disposed
  end
end
