defmodule Tenon.EventsTest do
  use ExUnit.Case, async: true

  alias Tenon.Ctx
  alias Tenon.Test.Helpers

  setup do
    kernel = start_supervised!({Tenon.Kernel, []})
    %{kernel: kernel, ctx: Tenon.Kernel.root(kernel)}
  end

  @tag :capture_log
  test "emit runs every hook in registration order and isolates failures", %{ctx: ctx} do
    me = self()
    Ctx.on(ctx, :ev, fn value -> send(me, {:hook, {:a, value}}) end)
    Ctx.on(ctx, :ev, fn _value -> raise "listener down" end)
    Ctx.on(ctx, :ev, fn value -> send(me, {:hook, {:b, value}}) end)

    assert Ctx.emit(ctx, :ev, [1]) == :ok
    assert Helpers.collected() == [{:a, 1}, {:b, 1}]
  end

  test "prepend puts a hook in front of the ones already registered", %{ctx: ctx} do
    me = self()
    Ctx.on(ctx, :ev, fn -> send(me, {:hook, :first}) end)
    Ctx.on(ctx, :ev, fn -> send(me, {:hook, :second}) end)
    Ctx.on(ctx, :ev, fn -> send(me, {:hook, :front}) end, prepend: true)
    Ctx.on(ctx, :ev, fn -> send(me, {:hook, :fronter}) end, prepend: true)

    Ctx.emit(ctx, :ev, [])

    assert Helpers.collected() == [:fronter, :front, :first, :second]
  end

  test "the disposer returned by on removes the hook", %{ctx: ctx} do
    me = self()
    off = Ctx.on(ctx, :ev, fn -> send(me, {:hook, :once}) end)

    Ctx.emit(ctx, :ev, [])
    off.()
    Ctx.emit(ctx, :ev, [])

    assert Helpers.collected() == [:once]
  end

  @tag :capture_log
  test "parallel runs every hook and collects failures", %{ctx: ctx} do
    me = self()
    Ctx.on(ctx, :ev, fn value -> send(me, {:hook, value}) end)
    Ctx.on(ctx, :ev, fn _value -> raise "boom" end)

    assert {:error, [%RuntimeError{message: "boom"}]} = Ctx.parallel(ctx, :ev, [:x])
    assert Helpers.collected() == [:x]
  end

  test "parallel returns ok when every hook succeeds", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn -> :ok end)

    assert Ctx.parallel(ctx, :ev, []) == :ok
    assert Ctx.parallel(ctx, :unknown, []) == :ok
  end

  test "serial stops at the first hook returning a value", %{ctx: ctx} do
    me = self()
    Ctx.on(ctx, :ev, fn _value -> send(me, {:hook, :one}) && nil end)
    Ctx.on(ctx, :ev, fn _value -> send(me, {:hook, :two}) && false end)
    Ctx.on(ctx, :ev, fn value -> send(me, {:hook, :three}) && {:ok, value * 2} end)
    Ctx.on(ctx, :ev, fn _value -> send(me, {:hook, :four}) end)

    assert Ctx.serial(ctx, :ev, [21]) == {:ok, 42}
    assert Helpers.collected() == [:one, :two, :three]
    assert Ctx.serial(ctx, :unknown, []) == nil
  end

  test "bail behaves like serial", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn -> nil end)
    Ctx.on(ctx, :ev, fn -> :decided end)

    assert Ctx.bail(ctx, :ev, []) == :decided
  end

  test "waterfall wraps the terminal and rewrites arguments", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn value, next -> next.(value + 1) end)
    Ctx.on(ctx, :ev, fn value, next -> next.(value * 2) end)

    assert Ctx.waterfall(ctx, :ev, [1], fn value -> {:terminal, value} end) == {:terminal, 4}
    assert Ctx.waterfall(ctx, :unknown, [7], fn value -> {:terminal, value} end) == {:terminal, 7}
  end

  test "a waterfall hook that skips next short-circuits the chain", %{ctx: ctx} do
    me = self()
    Ctx.on(ctx, :ev, fn _value, _next -> :vetoed end)
    Ctx.on(ctx, :ev, fn value, next -> send(me, {:hook, :downstream}) && next.(value) end)

    assert Ctx.waterfall(ctx, :ev, [1], fn value -> {:terminal, value} end) == :vetoed
    assert Helpers.collected() == []
  end

  test "a waterfall hook can rewrite the outer return value", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn value, next -> {:wrapped, next.(value)} end)

    assert Ctx.waterfall(ctx, :ev, [1], fn value -> {:terminal, value} end) ==
             {:wrapped, {:terminal, 1}}
  end

  test "waterfall carries several arguments through the chain", %{ctx: ctx} do
    Ctx.on(ctx, :ev, fn left, right, next -> next.(left * 2, right) end)
    Ctx.on(ctx, :ev, fn left, right, next -> next.(left, right <> "!") end)

    assert Ctx.waterfall(ctx, :ev, [2, "go"], fn left, right -> {left, right} end) == {4, "go!"}
  end

  test "waterfall refuses more arguments than it can wrap", %{ctx: ctx} do
    assert_raise ArgumentError, fn ->
      Ctx.waterfall(ctx, :ev, [1, 2, 3, 4, 5, 6], fn _a, _b, _c, _d, _e, _f -> :ok end)
    end
  end

  test "hooks disappear when their owner unloads", %{ctx: ctx} do
    {:ok, pid} = Ctx.plugin(ctx, Tenon.Test.Registrar, %{name: :thing, impl: :value})

    assert Ctx.bail(ctx, :ping, []) == :pong

    Tenon.Fiber.dispose(pid)

    assert Ctx.bail(ctx, :ping, []) == nil
  end
end
