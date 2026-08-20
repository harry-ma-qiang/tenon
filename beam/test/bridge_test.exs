defmodule Tenon.Beam.BridgeTest do
  use ExUnit.Case, async: false

  alias Tenon.Beam.Bus
  alias Tenon.Beam.Link
  alias Tenon.Beam.LogBridge
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
      halt: false
    }

    {:ok, _fiber} = :tenon.mount(ctx, %{module: Link, id: "link", config: config})

    on_exit(fn ->
      :tenon.stop(kernel)
      if Process.alive?(base), do: Base.shutdown(base)
    end)

    %{ctx: ctx}
  end

  test "envelope carries the closed-core defaults and the level map is stable" do
    envelope = Bus.envelope("guardian/pass", "info", "root", %{"target" => "root"})
    assert envelope["topic"] == "guardian/pass"
    assert envelope["env"] == "root"
    assert envelope["src"] == "beam"
    assert envelope["durable"] == true
    assert Bus.frame(envelope)["t"] == "bus.publish"
    assert Bus.level(:critical) == "error"
    assert Bus.level(:warning) == "warn"
    assert Bus.level(:notice) == "info"
    assert Bus.level(:debug) == "debug"
    assert Bus.level(:something_else) == "info"
  end

  test "the node publishes a lifecycle envelope right after registering" do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000

    assert_receive {:base, %{"t" => "bus.publish", "envelope" => envelope}}, 2_000
    assert envelope["topic"] == "guardian/node"
    assert envelope["env"] == "root"
    assert envelope["payload"]["event"] == "register"
    assert is_integer(envelope["payload"]["pid"])
  end

  test "the link publish service forwards an envelope to base's bus", %{ctx: ctx} do
    assert_receive {:base, %{"t" => "node.register"}}, 2_000

    envelope = Bus.envelope("log/root", "warn", "root", %{"msg" => "hi"})
    assert :tenon.svc(ctx, :link, :publish, [envelope]) == :ok

    assert_receive {:base, %{"t" => "bus.publish", "envelope" => %{"topic" => "log/root"} = got}},
                   2_000

    assert got["payload"]["msg"] == "hi"
    assert got["level"] == "warn"
  end

  test "the log bridge turns a logger event into a log/<node> envelope" do
    parent = self()
    publish = fn envelope -> send(parent, {:published, envelope}) end

    event = %{
      level: :warning,
      msg: {:string, "disk full"},
      meta: %{mfa: {SomeCaller, :run, 1}}
    }

    LogBridge.log(event, %{config: %{publish: publish, node: "root"}})

    assert_receive {:published, envelope}
    assert envelope["topic"] == "log/root"
    assert envelope["level"] == "warn"
    assert envelope["payload"]["msg"] == "disk full"
    assert envelope["payload"]["level"] == "warning"
  end

  test "the log bridge drops events raised by the bridge itself" do
    parent = self()
    publish = fn envelope -> send(parent, {:published, envelope}) end

    event = %{
      level: :error,
      msg: {:string, "internal send failed"},
      meta: %{mfa: {Tenon.Beam.Link.Server, :send_frame, 2}}
    }

    LogBridge.log(event, %{config: %{publish: publish, node: "root"}})

    refute_receive {:published, _envelope}, 200
  end
end
