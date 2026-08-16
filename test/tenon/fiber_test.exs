defmodule Tenon.FiberTest do
  use ExUnit.Case, async: true

  alias Tenon.Ctx
  alias Tenon.Fiber
  alias Tenon.Test.Helpers

  setup do
    kernel = start_supervised!({Tenon.Kernel, []})
    %{kernel: kernel, ctx: Tenon.Kernel.root(kernel)}
  end

  test "a plugin loads on mount and unloads on dispose", %{kernel: kernel, ctx: ctx} do
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Echo, %{pid: self(), tag: :a})

    assert_receive {:loaded, :a}
    assert Fiber.status(pid) == :active

    assert Fiber.dispose(pid) == :ok
    assert_receive {:disposed, :a}
    refute Process.alive?(pid)
    assert %{children: []} = Tenon.Kernel.tree(kernel)
  end

  test "disposers run in reverse registration order", %{ctx: ctx} do
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Stack, %{pid: self(), tags: [:one, :two, :three]})

    Fiber.dispose(pid)

    assert Helpers.collected() == [:three, :two, :one]
  end

  test "a single effect disposer removes only its own effect", %{ctx: ctx} do
    me = self()
    first = Ctx.effect(ctx, fn -> fn -> send(me, {:hook, :first}) end end)
    _second = Ctx.effect(ctx, fn -> fn -> send(me, {:hook, :second}) end end)

    first.()

    assert Helpers.collected() == [:first]
  end

  test "an effect body that returns nil registers nothing", %{ctx: ctx} do
    disposer = Ctx.effect(ctx, fn -> nil end)

    assert disposer.() == :ok
  end

  @tag :capture_log
  test "apply that raises leaves the fiber failed and restartable", %{kernel: kernel, ctx: ctx} do
    {:ok, agent} = Agent.start_link(fn -> true end)
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Boom, %{pid: self(), agent: agent})

    assert Fiber.status(pid) == :failed
    assert Process.alive?(pid)

    assert %{children: [%{status: :failed, error: {%RuntimeError{}, _stack}}]} =
             Tenon.Kernel.tree(kernel)

    Agent.update(agent, fn _fail? -> false end)
    assert Fiber.restart(pid) == :ok
    assert Fiber.status(pid) == :active
    assert_receive {:loaded, :boom}
  end

  test "update reloads the plugin with the new config", %{ctx: ctx} do
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Echo, %{pid: self(), tag: :old})
    assert_receive {:loaded, :old}

    assert Fiber.update(pid, %{pid: self(), tag: :new}) == :ok

    assert_receive {:disposed, :old}
    assert_receive {:loaded, :new}
    assert Fiber.status(pid) == :active
  end

  test "unloading a parent disposes its children in reverse effect order", %{ctx: ctx} do
    {:ok, parent} = Ctx.plugin(ctx, Tenon.Test.Parent, %{pid: self()})
    assert_receive {:child, child}

    Fiber.dispose(parent)

    assert Helpers.collected() == [:after_child, :child, :before_child]
    refute Process.alive?(child)
  end

  test "killing a parent disposes its children", %{ctx: ctx} do
    {:ok, parent} = Ctx.plugin(ctx, Tenon.Test.Parent, %{pid: self()})
    assert_receive {:child, child}

    Process.exit(parent, :kill)
    Helpers.wait_until(fn -> not Process.alive?(child) end)

    assert :ets.lookup(ctx.tables.fibers, child) == []
  end
end
