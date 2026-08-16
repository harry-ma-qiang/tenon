defmodule Tenon.Test.Adversarial.Rich do
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.on(ctx, :ping, fn -> :pong end)
    Tenon.Ctx.provide(ctx, config.name, config.impl)
    {:ok, child} = Tenon.Ctx.plugin(ctx, Tenon.Test.Echo, %{pid: config.pid, tag: :rich_child})
    send(config.pid, {:child, child})
    :ok
  end
end

defmodule Tenon.Test.Adversarial.Blocker do
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.on(ctx, :ping, fn -> :pong end)
    Tenon.Ctx.provide(ctx, config.name, config.impl)
    send(config.pid, :ready)

    receive do
      :proceed -> :ok
    end
  end
end

defmodule Tenon.Test.Adversarial.RaisyStack do
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.effect(ctx, fn -> fn -> send(config.pid, {:hook, :one}) end end)
    Tenon.Ctx.effect(ctx, fn -> fn -> raise "disposer boom" end end)
    Tenon.Ctx.effect(ctx, fn -> fn -> send(config.pid, {:hook, :three}) end end)
    :ok
  end
end

defmodule Tenon.Test.Adversarial.ForeignRegistrar do
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.on(ctx, :ev, fn -> register_on_target(ctx, config) end)
    :ok
  end

  defp register_on_target(ctx, config) do
    target = %Tenon.Ctx{kernel: ctx.kernel, tables: ctx.tables, fiber: config.target}
    Tenon.Ctx.effect(target, fn -> notify_foreign(config.pid) end)
  end

  defp notify_foreign(pid), do: fn -> send(pid, :disposed_via_foreign) end
end

defmodule Tenon.Test.Adversarial.OrderConsumer do
  use Tenon.Plugin

  @impl Tenon.Plugin
  def inject, do: [:db]

  @impl Tenon.Plugin
  def apply(ctx, config) do
    send(config.pid, {:consumer_loaded, Tenon.Ctx.get(ctx, :db)})

    Tenon.Ctx.effect(ctx, fn ->
      fn -> send(config.pid, {:consumer_unloading, Tenon.Ctx.get(ctx, :db)}) end
    end)

    :ok
  end
end

defmodule Tenon.Test.Adversarial.PartialBoom do
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.effect(ctx, fn -> fn -> send(config.pid, {:hook, :partial}) end end)
    if Agent.get(config.agent, & &1), do: raise("boom")
    :ok
  end
end

defmodule Tenon.Test.Adversarial.SelfStatusHook do
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.on(ctx, :probe, fn -> send(config.pid, {:status, Tenon.Fiber.status(self())}) end)
    Tenon.Ctx.emit(ctx, :probe, [])
    send(config.pid, :apply_finished)
    :ok
  end
end

defmodule Tenon.AdversarialTest do
  use ExUnit.Case, async: true

  alias Tenon.Ctx
  alias Tenon.Fiber
  alias Tenon.Test.Helpers

  setup do
    kernel = start_supervised!({Tenon.Kernel, []})
    %{kernel: kernel, ctx: Tenon.Kernel.root(kernel)}
  end

  test "killing a fiber with hooks, a service and a child clears every row and pends dependents",
       %{ctx: ctx} do
    {:ok, provider} =
      Ctx.plugin(ctx, Tenon.Test.Adversarial.Rich, %{pid: self(), name: :db, impl: :conn})

    assert_receive {:child, child}

    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: self(), tag: :c})
    assert Fiber.status(consumer) == :active

    Process.exit(provider, :kill)
    Helpers.wait_until(fn -> :ets.lookup(ctx.tables.fibers, provider) == [] end)
    Helpers.wait_until(fn -> not Process.alive?(child) end)

    assert :ets.lookup(ctx.tables.fibers, child) == []
    assert :ets.match_object(ctx.tables.hooks, {{:_, :_}, :_, provider, :_}) == []
    assert :ets.match_object(ctx.tables.services, {:_, :_, provider}) == []
    assert Ctx.get(ctx, :db) == nil

    Helpers.wait_until(fn -> Fiber.status(consumer) == :pending end)
    assert_receive {:consumer_unloaded, :c}
  end

  test "killing a fiber blocked mid apply still sweeps its live registrations", %{ctx: ctx} do
    {:ok, pid} =
      Tenon.Kernel.start_fiber(
        ctx.kernel,
        Tenon.Test.Adversarial.Blocker,
        %{pid: self(), name: :blocked_db, impl: :x},
        parent: ctx.fiber
      )

    assert_receive :ready

    assert :ets.match_object(ctx.tables.hooks, {{:_, :_}, :_, pid, :_}) != []
    assert :ets.match_object(ctx.tables.services, {:_, :_, pid}) != []

    Process.exit(pid, :kill)
    Helpers.wait_until(fn -> :ets.lookup(ctx.tables.fibers, pid) == [] end)

    assert :ets.match_object(ctx.tables.hooks, {{:_, :_}, :_, pid, :_}) == []
    assert :ets.match_object(ctx.tables.services, {:_, :_, pid}) == []
    assert Ctx.get(ctx, :blocked_db) == nil
  end

  @tag :capture_log
  test "a raising disposer does not stop the remaining disposers from running", %{ctx: ctx} do
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Adversarial.RaisyStack, %{pid: self()})

    assert Fiber.dispose(pid) == :ok

    assert Helpers.collected() == [:three, :one]
    refute Process.alive?(pid)
  end

  test "a raising hook inside waterfall propagates to the caller", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn _value, _next -> raise "waterfall boom" end)

    assert_raise RuntimeError, "waterfall boom", fn ->
      Ctx.waterfall(ctx, :ev, [1], fn value -> {:terminal, value} end)
    end
  end

  test "a raising hook inside serial propagates to the caller", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn -> raise "serial boom" end)

    assert_raise RuntimeError, "serial boom", fn -> Ctx.serial(ctx, :ev, []) end
  end

  test "waterfall works with zero arguments", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn next -> next.() end)

    assert Ctx.waterfall(ctx, :ev, [], fn -> :terminal end) == :terminal
  end

  test "waterfall carries five arguments through the chain", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn a, b, c, d, e, next -> next.(a + 1, b, c, d, e) end)

    assert Ctx.waterfall(ctx, :ev, [1, 2, 3, 4, 5], fn a, b, c, d, e -> {a, b, c, d, e} end) ==
             {2, 2, 3, 4, 5}
  end

  test "an effect registered from a hook running in a foreign process lands on the named fiber",
       %{ctx: ctx} do
    {:ok, fiber_y} = Ctx.plugin(ctx, Tenon.Test.Echo, %{pid: self(), tag: :y})
    assert_receive {:loaded, :y}

    {:ok, _fiber_x} =
      Ctx.plugin(ctx, Tenon.Test.Adversarial.ForeignRegistrar, %{pid: self(), target: fiber_y})

    Ctx.emit(ctx, :ev, [])

    Fiber.dispose(fiber_y)

    assert_receive :disposed_via_foreign
    assert_receive {:disposed, :y}
  end

  test "the service row is gone before the dependent's unload disposer observes it", %{ctx: ctx} do
    {:ok, db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})

    {:ok, _consumer} =
      Ctx.plugin(ctx, Tenon.Test.Adversarial.OrderConsumer, %{pid: self()})

    assert_receive {:consumer_loaded, :conn}

    Fiber.dispose(db)

    assert_receive {:consumer_unloading, seen}
    assert seen == nil
  end

  test "a provider swap reloads the dependent exactly once with the new impl", %{ctx: ctx} do
    {:ok, db_a} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :a})
    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: self(), tag: :c})
    assert_receive {:consumer_loaded, :a}

    Fiber.dispose(db_a)
    assert_receive {:consumer_unloaded, :c}

    {:ok, _db_b} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :b})

    assert_receive {:consumer_loaded, :b}
    refute_receive {:consumer_loaded, _impl}, 100
    assert Fiber.status(consumer) == :active
    assert Ctx.get(ctx, :db) == :b
  end

  test "update while pending stores the config used once it later loads", %{ctx: ctx} do
    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: self(), tag: :first})
    assert Fiber.status(consumer) == :pending

    assert Fiber.update(consumer, %{pid: self(), tag: :second}) == :ok
    assert Fiber.status(consumer) == :pending

    {:ok, _db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})

    assert Fiber.status(consumer) == :active
    assert_receive {:consumer_loaded, :conn}

    Fiber.dispose(consumer)
    assert_receive {:consumer_unloaded, :second}
  end

  @tag :capture_log
  test "effects registered before a failed load survive until the next unload", %{ctx: ctx} do
    {:ok, agent} = Agent.start_link(fn -> true end)

    {:ok, pid} =
      Ctx.plugin(ctx, Tenon.Test.Adversarial.PartialBoom, %{pid: self(), agent: agent})

    assert Fiber.status(pid) == :failed
    refute_received {:hook, :partial}

    assert Fiber.restart(pid) == :ok
    assert Fiber.status(pid) == :failed
    assert_receive {:hook, :partial}
    refute_received {:hook, :partial}

    Agent.update(agent, fn _fail? -> false end)
    assert Fiber.restart(pid) == :ok
    assert Fiber.status(pid) == :active
    assert_receive {:hook, :partial}
    refute_received {:hook, :partial}
  end

  test "disposing a fiber twice is safe", %{ctx: ctx} do
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Echo, %{pid: self(), tag: :a})
    assert_receive {:loaded, :a}

    assert Fiber.dispose(pid) == :ok
    assert Fiber.dispose(pid) == :ok
  end

  test "a consumer injecting a service provided only in another kernel never loads", %{ctx: one} do
    two = Tenon.Kernel.root(start_supervised!({Tenon.Kernel, []}, id: :adversarial_two))

    {:ok, _db} = Ctx.plugin(one, Tenon.Test.Db, %{impl: :conn})
    {:ok, consumer} = Ctx.plugin(two, Tenon.Test.Consumer, %{pid: self(), tag: :c})

    assert Fiber.status(consumer) == :pending
    refute_received {:consumer_loaded, _impl}
  end

  test "mounting and disposing 500 fibers leaves no residue", %{kernel: kernel, ctx: ctx} do
    sup = :sys.get_state(kernel).sup
    baseline_fibers = :ets.info(ctx.tables.fibers, :size)
    baseline_hooks = :ets.info(ctx.tables.hooks, :size)
    baseline_services = :ets.info(ctx.tables.services, :size)
    baseline_children = DynamicSupervisor.count_children(sup).active

    Enum.each(1..500, fn i ->
      {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Registrar, %{name: :"svc_#{i}", impl: i})
      Fiber.dispose(pid)
    end)

    assert :ets.info(ctx.tables.fibers, :size) == baseline_fibers
    assert :ets.info(ctx.tables.hooks, :size) == baseline_hooks
    assert :ets.info(ctx.tables.services, :size) == baseline_services
    assert DynamicSupervisor.count_children(sup).active == baseline_children
  end

  @tag :capture_log
  test "a hook calling Fiber.status on its own currently-emitting fiber fails fast, not a deadlock",
       %{ctx: ctx} do
    me = self()

    task =
      Task.async(fn ->
        Ctx.plugin(ctx, Tenon.Test.Adversarial.SelfStatusHook, %{pid: me})
      end)

    assert {:ok, {:ok, pid}} = Task.yield(task, 2_000) || Task.shutdown(task, :brutal_kill)
    assert Fiber.status(pid) == :active
    assert_receive :apply_finished
    refute_received {:status, _reply}
  end

  test "internal/plugin is only emitted on mount, not on dispose", %{ctx: ctx} do
    me = self()
    Ctx.on(ctx, :"internal/plugin", fn fiber -> send(me, {:plugin_event, fiber}) end)

    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Echo, %{pid: self(), tag: :a})
    assert_receive {:plugin_event, ^pid}

    Fiber.dispose(pid)
    refute_receive {:plugin_event, ^pid}, 100
  end
end
