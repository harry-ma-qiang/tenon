defmodule Tenon.InjectTest do
  use ExUnit.Case, async: true

  alias Tenon.Ctx
  alias Tenon.Fiber
  alias Tenon.Test.Helpers

  setup do
    kernel = start_supervised!({Tenon.Kernel, []})
    %{kernel: kernel, ctx: Tenon.Kernel.root(kernel)}
  end

  test "a fiber waits for its injected service, then loads", %{ctx: ctx} do
    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: self(), tag: :c})

    assert Fiber.status(consumer) == :pending
    refute_received {:consumer_loaded, _impl}

    {:ok, _db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})

    assert Fiber.status(consumer) == :active
    assert_receive {:consumer_loaded, :conn}
  end

  test "losing the provider unloads the dependent, regaining it reloads", %{ctx: ctx} do
    {:ok, db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})
    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: self(), tag: :c})

    assert Fiber.status(consumer) == :active
    assert_receive {:consumer_loaded, :conn}

    Fiber.dispose(db)

    assert Fiber.status(consumer) == :pending
    assert_receive {:consumer_unloaded, :c}
    assert Ctx.get(ctx, :db) == nil

    {:ok, _db2} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn2})

    assert Fiber.status(consumer) == :active
    assert_receive {:consumer_loaded, :conn2}
  end

  test "swapping the provider changes the epoch of the dependent", %{ctx: ctx} do
    {:ok, db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})
    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: self(), tag: :c})

    assert Fiber.status(consumer) == :active
    assert epoch(ctx, consumer) == [{:db, db}]

    Fiber.dispose(db)
    assert Fiber.status(consumer) == :pending
    assert epoch(ctx, consumer) == :inactive

    {:ok, db2} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn2})

    assert Fiber.status(consumer) == :active
    assert epoch(ctx, consumer) == [{:db, db2}]
    refute db2 == db
  end

  test "killing the provider unloads the dependent", %{ctx: ctx} do
    {:ok, db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})
    {:ok, consumer} = Ctx.plugin(ctx, Tenon.Test.Consumer, %{pid: self(), tag: :c})

    assert Fiber.status(consumer) == :active

    Process.exit(db, :kill)
    Helpers.wait_until(fn -> Ctx.get(ctx, :db) == nil end)

    assert Fiber.status(consumer) == :pending
    assert_receive {:consumer_unloaded, :c}
  end

  test "a service is withdrawn when its provider unloads", %{ctx: ctx} do
    {:ok, db} = Ctx.plugin(ctx, Tenon.Test.Db, %{impl: :conn})

    assert Ctx.get(ctx, :db) == :conn

    Fiber.dispose(db)

    assert Ctx.get(ctx, :db) == nil
  end

  defp epoch(ctx, fiber) do
    [row] = :ets.lookup(ctx.tables.fibers, fiber)
    elem(row, 7)
  end
end
