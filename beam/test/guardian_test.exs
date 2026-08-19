defmodule Tenon.Beam.GuardianTest do
  use ExUnit.Case, async: false

  alias Tenon.Beam.Guardian
  alias Tenon.Beam.Link
  alias Tenon.Beam.Test.Base

  @probe_timeout 400

  setup do
    {base, path} = Base.start(self())
    healthy(base)
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

  defp healthy(base) do
    Base.answer(base, "health", {:ok, %{"ok" => true}})
    Base.answer(base, "tree", {:ok, %{"tree" => %{"status" => "active"}}})
    Base.answer(base, {"svc", "worker"}, {:ok, "pong"})
    Base.answer(base, {"svc", "loop"}, {:ok, "pong"})
    Base.answer(base, "status", {:ok, %{"nodes" => [env_row()]}})
    Base.answer(base, "events.tail", {:ok, %{"events" => []}})
    Base.answer(base, "reset", {:ok, %{"ok" => true}})
  end

  defp env_row(extra \\ %{}) do
    Map.merge(
      %{
        "env" => "root",
        "worker" => %{"state" => "ready"},
        "harness" => %{"state" => "ready"},
        "budget" => %{}
      },
      extra
    )
  end

  defp watch(ctx, opts) do
    config =
      Map.merge(
        %{
          target: "root",
          interval: 20,
          failures: 3,
          probe_timeout: @probe_timeout,
          notify: self()
        },
        opts
      )

    {:ok, fiber} = :tenon.mount(ctx, %{module: Guardian, id: "guardian", config: config})
    assert :tenon.status(fiber) == :active
    fiber
  end

  defp fails_with(ctx, name) do
    watch(ctx, %{})
    assert_receive {:tenon_guardian, :failed, names}, 5_000
    assert name in names, "expected #{name} among #{inspect(names)}"
  end

  test "base itself: no status answer is a failing probe", %{base: base, ctx: ctx} do
    Base.answer(base, "status", {:error, "gone"})
    fails_with(ctx, "base")
  end

  test "an env whose worker is still booting is not failing", %{base: base, ctx: ctx} do
    booting = %{"nodes" => [env_row(%{"worker" => %{"state" => "booting"}})]}
    Base.answer(base, "status", {:ok, booting})
    Base.answer(base, {"svc", "worker"}, {:error, "no worker yet"})
    watch(ctx, %{})
    assert_receive {:tenon_guardian, :up}, 2_000
  end

  test "an env whose worker base reports as failed is failing", %{base: base, ctx: ctx} do
    failed = %{"nodes" => [env_row(%{"worker" => %{"state" => "failed", "error" => "boom"}})]}
    Base.answer(base, "status", {:ok, failed})
    fails_with(ctx, "worker")
  end

  test "stays quiet while every core probe answers", %{ctx: ctx} do
    watch(ctx, %{})
    assert_receive {:tenon_guardian, :up}, 2_000
    refute_receive {:tenon_guardian, :reset}, 300
  end

  test "env alive: an unhealthy answer is a failing probe", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:ok, %{"ok" => false}})
    fails_with(ctx, "env")
  end

  test "kernel tree healthy: a root fiber that is not active fails", %{base: base, ctx: ctx} do
    Base.answer(base, "tree", {:ok, %{"tree" => %{"status" => "failed"}}})
    fails_with(ctx, "tree")
  end

  test "worker responsive: no pong from worker.ping fails", %{base: base, ctx: ctx} do
    Base.answer(base, {"svc", "worker"}, {:error, "no worker"})
    fails_with(ctx, "worker")
  end

  test "harness responsive: no pong from loop.ping fails", %{base: base, ctx: ctx} do
    Base.answer(base, {"svc", "loop"}, {:ok, "who?"})
    fails_with(ctx, "harness")
  end

  test "budgets: a halted env fails the budget probe", %{base: base, ctx: ctx} do
    halted = %{"nodes" => [env_row(%{"budget" => %{"halted" => "budget tokens"}})]}
    Base.answer(base, "status", {:ok, halted})
    fails_with(ctx, "budgets")
  end

  test "violations: a violation row in the log fails, once", %{base: base, ctx: ctx} do
    rows = %{"events" => [%{"id" => 7, "kind" => "budget.exceeded"}]}
    Base.answer(base, "events.tail", {:ok, rows})
    watch(ctx, %{failures: 99})
    assert_receive {:tenon_guardian, :failed, names}, 5_000
    assert "violations" in names
    Base.answer(base, "events.tail", {:ok, %{"events" => []}})
    assert_receive {:tenon_guardian, :up}, 5_000
  end

  test "wedged waits: a probe that outlives probe_timeout fails as wedged", %{
    base: base,
    ctx: ctx
  } do
    Base.answer(base, "tree", :ignore)
    watch(ctx, %{})
    assert_receive {:tenon_guardian, :failed, names}, 5_000
    assert "wedged" in names, inspect(names)
  end

  test "extra probes: an approved executable that exits non-zero fails", %{ctx: ctx} do
    bad = script("exit 3")
    good = script("exit 0")
    watch(ctx, %{probes: [good, bad], failures: 99})
    assert_receive {:tenon_guardian, :failed, names}, 5_000
    assert Path.basename(bad) in names
    refute Path.basename(good) in names
  end

  test "resets the env after N passes and names the failing probes", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:error, "down"})
    watch(ctx, %{failures: 2})
    assert_receive {:tenon_guardian, :strike, 1}, 5_000
    assert_receive {:tenon_guardian, :reset}, 5_000
    assert_receive {:base, %{"t" => "reset", "env" => "root", "probes" => probes}}, 5_000
    assert "env" in probes
  end

  test "a recovery clears the strike count", %{base: base, ctx: ctx} do
    Base.answer(base, "health", {:error, "down"})
    watch(ctx, %{failures: 4})
    assert_receive {:tenon_guardian, :strike, 2}, 5_000
    healthy(base)
    assert_receive {:tenon_guardian, :up}, 5_000
    refute_receive {:tenon_guardian, :reset}, 300
  end

  test "probes the env base named as the target", %{ctx: ctx} do
    watch(ctx, %{target: "child-1"})
    assert_receive {:base, %{"t" => "health", "env" => "child-1"}}, 2_000
  end

  defp script(body) do
    path = Path.join(System.tmp_dir!(), "probe-#{System.unique_integer([:positive])}.sh")
    File.write!(path, "#!/bin/sh\n#{body}\n")
    File.chmod!(path, 0o755)
    on_exit(fn -> File.rm(path) end)
    path
  end
end
