defmodule Tenon.Beam.LinkTest do
  use ExUnit.Case, async: false

  alias Tenon.Beam.Link
  alias Tenon.Beam.Test.Base

  setup do
    {base, path} = Base.start(self())
    {:ok, kernel} = :tenon.start()
    ctx = :tenon.root(kernel)

    {:ok, loader} =
      :tenon.mount(ctx, %{module: Tenon.Loader, id: "loader", config: %{layers: []}})

    config = %{
      sock: path,
      role: "agent",
      env: "root",
      kernel: kernel,
      loader: loader,
      halt: false,
      notify: self()
    }

    {:ok, fiber} = :tenon.mount(ctx, %{module: Link, id: "link", config: config})

    on_exit(fn ->
      :tenon.stop(kernel)
      if Process.alive?(base), do: Base.shutdown(base)
    end)

    %{base: base, ctx: ctx, kernel: kernel, fiber: fiber}
  end

  test "registers on connect", %{fiber: fiber} do
    assert :tenon.status(fiber) == :active
    assert_receive {:base, frame}, 2_000
    assert frame["t"] == "node.register"
    assert frame["role"] == "agent"
    assert frame["env"] == "root"
    assert is_integer(frame["pid"])
  end

  test "answers health", %{base: base} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    Base.push(base, %{"t" => "health", "id" => 7})
    assert_receive {:base, %{"t" => "rep", "id" => 7, "result" => result}}, 2_000
    assert result["ok"]
    assert result["role"] == "agent"
    assert result["env"] == "root"
    assert result["failed"] == 0
    assert result["fibers"] >= 3
  end

  test "answers tree", %{base: base} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    Base.push(base, %{"t" => "tree", "id" => 8})
    assert_receive {:base, %{"t" => "rep", "id" => 8, "result" => %{"tree" => tree}}}, 2_000
    ids = Enum.map(tree["children"], & &1["id"])
    assert "link" in ids
    assert "loader" in ids
  end

  test "answers reload", %{base: base} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    Base.push(base, %{"t" => "reload", "id" => 9})
    assert_receive {:base, %{"t" => "rep", "id" => 9, "result" => %{"ok" => true}}}, 2_000
  end

  test "proxies svc to a kernel service", %{base: base, ctx: ctx} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    :tenon.provide(ctx, :probe, fn :ping, [] -> "pong" end)
    Base.push(base, %{
      "t" => "svc",
      "id" => 11,
      "name" => "probe",
      "method" => "ping",
      "args" => []
    })

    assert_receive {:base, %{"t" => "rep", "id" => 11, "result" => "pong"}}, 2_000
  end

  test "reports an unknown svc as an error", %{base: base} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000

    Base.push(base, %{
      "t" => "svc",
      "id" => 12,
      "name" => "nope",
      "method" => "ping",
      "args" => []
    })

    assert_receive {:base, %{"t" => "rep", "id" => 12, "error" => _reason}}, 2_000
  end

  test "refuses an unknown method", %{base: base} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    Base.push(base, %{"t" => "nope", "id" => 10})
    assert_receive {:base, %{"t" => "rep", "id" => 10, "error" => "unknown_method:nope"}}, 2_000
  end

  test "correlates a request this node made", %{base: base, ctx: ctx} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    task = Task.async(fn -> :tenon.svc(ctx, :link, :request, ["ping", %{"x" => 1}]) end)
    assert_receive {:base, %{"t" => "ping", "id" => id, "x" => 1}}, 2_000
    Base.push(base, %{"t" => "rep", "id" => id, "result" => "pong"})
    assert Task.await(task) == {:ok, "pong"}
  end

  test "reports an error reply as an error", %{base: base, ctx: ctx} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    task = Task.async(fn -> :tenon.svc(ctx, :link, :request, ["ping", %{}]) end)
    assert_receive {:base, %{"t" => "ping", "id" => id}}, 2_000
    Base.push(base, %{"t" => "rep", "id" => id, "error" => "no_such_env"})
    assert Task.await(task) == {:error, "no_such_env"}
  end

  test "stops the node when base goes away", %{base: base} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000
    Base.close(base)
    assert_receive {:tenon_link, :down, :closed}, 2_000
  end

  test "fails to load without a base socket" do
    {:ok, kernel} = :tenon.start()
    ctx = :tenon.root(kernel)
    config = %{sock: "/nonexistent/tenon-base.sock", role: "agent", env: "root"}
    {:ok, fiber} = :tenon.mount(ctx, %{module: Link, id: "link", config: config})
    assert :tenon.status(fiber) == :failed
    :tenon.stop(kernel)
  end
end
