defmodule Tenon.InternalEventsTest do
  use ExUnit.Case, async: true

  alias Tenon.Ctx
  alias Tenon.Fiber

  setup do
    kernel = start_supervised!({Tenon.Kernel, []})
    %{kernel: kernel, ctx: Tenon.Kernel.root(kernel)}
  end

  test "fibers announce themselves, their status and their services", %{ctx: ctx} do
    me = self()
    Ctx.on(ctx, :"internal/plugin", fn fiber -> send(me, {:plugin, fiber}) end)

    Ctx.on(ctx, :"internal/status", fn fiber, old, new -> send(me, {:status, fiber, old, new}) end)

    Ctx.on(ctx, :"internal/service", fn name, impl -> send(me, {:service, name, impl}) end)

    {:ok, db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})

    assert_receive {:plugin, ^db}
    assert_receive {:status, ^db, :pending, :loading}
    assert_receive {:status, ^db, :loading, :active}
    assert_receive {:service, :db, :conn}

    Fiber.dispose(db)

    assert_receive {:status, ^db, :active, :unloading}
    assert_receive {:status, ^db, :unloading, :disposed}
    assert_receive {:service, :db, nil}
  end

  test "a dependent announces the unload caused by a lost service", %{ctx: ctx} do
    me = self()
    {:ok, db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})
    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: me, tag: :c})
    assert Fiber.status(consumer) == :active

    Ctx.on(ctx, :"internal/status", fn fiber, old, new -> send(me, {:status, fiber, old, new}) end)

    Fiber.dispose(db)
    assert Fiber.status(consumer) == :pending

    assert_receive {:status, ^consumer, :active, :unloading}
    assert_receive {:status, ^consumer, :unloading, :pending}
  end
end
