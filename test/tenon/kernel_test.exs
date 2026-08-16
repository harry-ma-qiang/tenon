defmodule Tenon.KernelTest do
  use ExUnit.Case, async: true

  alias Tenon.Ctx
  alias Tenon.Test.Helpers

  setup do
    kernel = start_supervised!({Tenon.Kernel, []})
    %{kernel: kernel, ctx: Tenon.Kernel.root(kernel)}
  end

  test "root returns a ctx owned by a live root fiber", %{kernel: kernel, ctx: ctx} do
    assert %Ctx{kernel: ^kernel, fiber: fiber, parent: nil} = ctx
    assert Process.alive?(fiber)
    assert Tenon.Fiber.status(fiber) == :active
  end

  test "tree shows the root fiber and its children", %{kernel: kernel, ctx: ctx} do
    assert %{module: nil, status: :active, children: []} = Tenon.Kernel.tree(kernel)

    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Echo, %{pid: self(), tag: :a})

    assert %{children: [%{pid: ^pid, module: Tenon.Test.Echo, status: :active, parent: parent}]} =
             Tenon.Kernel.tree(kernel)

    assert parent == ctx.fiber
  end

  test "killing a fiber leaves no hook, service or status rows", %{ctx: ctx} do
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Registrar, %{name: :thing, impl: :value})

    assert Ctx.get(ctx, :thing) == :value
    assert Ctx.bail(ctx, :ping, []) == :pong

    Process.exit(pid, :kill)
    Helpers.wait_until(fn -> :ets.lookup(ctx.tables.fibers, pid) == [] end)

    assert Ctx.get(ctx, :thing) == nil
    assert Ctx.bail(ctx, :ping, []) == nil
    assert :ets.match_object(ctx.tables.hooks, {{:_, :_}, :_, pid, :_}) == []
    assert :ets.match_object(ctx.tables.services, {:_, :_, pid}) == []
  end

  test "two kernels share nothing", %{ctx: one} do
    other = Tenon.Kernel.root(start_supervised!({Tenon.Kernel, []}, id: :second))

    {:ok, _pid} = Ctx.plugin(one, Tenon.Test.Registrar, %{name: :thing, impl: :value})
    Ctx.on(other, :ping, fn -> :other end)

    assert Ctx.get(one, :thing) == :value
    assert Ctx.get(other, :thing) == nil
    assert Ctx.bail(one, :ping, []) == :pong
    assert Ctx.bail(other, :ping, []) == :other
    assert one.tables.hooks != other.tables.hooks
    assert %{children: []} = Tenon.Kernel.tree(other.kernel)
  end

  test "a named kernel is reachable by name" do
    name = :"kernel_#{System.unique_integer([:positive])}"
    kernel = start_supervised!({Tenon.Kernel, [name: name]}, id: :named)

    assert Process.whereis(name) == kernel
    assert %Ctx{kernel: ^kernel} = Tenon.Kernel.root(name)
  end
end
