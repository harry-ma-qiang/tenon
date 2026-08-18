defmodule Tenon.Beam.GuardianTest do
  use ExUnit.Case, async: false

  alias Tenon.Beam.Guardian
  alias Tenon.Beam.Link
  alias Tenon.Beam.Test.Base

  setup do
    {base, path} = Base.start(self())
    Base.answer(base, "reset", {:ok, %{"ok" => true}})
    {:ok, kernel} = :tenon.start()
    ctx = :tenon.root(kernel)

    link = %{sock: path, role: "guardian", env: "guardian", kernel: kernel, halt: false}
    {:ok, _fiber} = :tenon.mount(ctx, %{module: Link, id: "link", config: link})
    assert_receive {:base, %{"t" => "node.register", "role" => "guardian"}}, 2_000

    on_exit(fn ->
      :tenon.stop(kernel)
      if Process.alive?(base), do: Base.shutdown(base)
    end)

    %{base: base, ctx: ctx}
  end

  defp watch(ctx, opts) do
    config = Map.merge(%{target: "root", interval: 20, failures: 3, notify: self()}, opts)
    {:ok, fiber} = :tenon.mount(ctx, %{module: Guardian, id: "guardian", config: config})
    assert :tenon.status(fiber) == :active
    fiber
  end

  test "stays quiet while the env is healthy", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:ok, %{"ok" => true}})
    watch(ctx, %{})
    assert_receive {:tenon_guardian, :up}, 2_000
    refute_receive {:tenon_guardian, :reset}, 300
  end

  test "resets the env after the configured number of failures", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:error, "down"})
    watch(ctx, %{})
    assert_receive {:tenon_guardian, :strike, 1}, 2_000
    assert_receive {:tenon_guardian, :strike, 2}, 2_000
    assert_receive {:tenon_guardian, :reset}, 2_000
    assert_receive {:base, %{"t" => "reset", "env" => "root"}}, 2_000
  end

  test "counts an unhealthy answer as a failure", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:ok, %{"ok" => false}})
    watch(ctx, %{failures: 2})
    assert_receive {:tenon_guardian, :strike, 1}, 2_000
    assert_receive {:tenon_guardian, :reset}, 2_000
  end

  test "a recovery clears the strike count", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:error, "down"})
    watch(ctx, %{failures: 4})
    assert_receive {:tenon_guardian, :strike, 2}, 2_000
    Base.answer(base, "health", {:ok, %{"ok" => true}})
    assert_receive {:tenon_guardian, :up}, 2_000
    refute_receive {:tenon_guardian, :reset}, 300
  end

  test "probes the env base named as the target", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:ok, %{"ok" => true}})
    watch(ctx, %{target: "child-1"})
    assert_receive {:base, %{"t" => "health", "env" => "child-1"}}, 2_000
  end
end
