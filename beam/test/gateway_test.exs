defmodule Tenon.Beam.GatewayTest do
  use ExUnit.Case, async: false

  alias Tenon.Beam.Frame
  alias Tenon.Beam.Gateway

  setup do
    path =
      Path.join(System.tmp_dir!(), "tenon-gateway-#{System.unique_integer([:positive])}.sock")

    {:ok, kernel} = :tenon.start()
    ctx = :tenon.root(kernel)

    {:ok, fiber} =
      :tenon.mount(ctx, %{
        module: Gateway,
        id: "gateway",
        config: %{address: "unix:" <> path}
      })

    on_exit(fn -> :tenon.stop(kernel) end)

    %{ctx: ctx, kernel: kernel, fiber: fiber, path: path}
  end

  defp connect(path) do
    {:ok, socket} =
      :gen_tcp.connect({:local, to_charlist(path)}, 0, [:binary, {:packet, 4}, active: false])

    socket
  end

  defp send_frame(socket, frame), do: :gen_tcp.send(socket, Frame.encode(frame))

  defp recv_frame(socket) do
    {:ok, body} = :gen_tcp.recv(socket, 0, 2_000)
    Frame.decode(body)
  end

  defp handshake(socket, name) do
    send_frame(socket, %{"t" => "hello", "inject" => []})
    {:ok, %{"t" => "load", "req" => req}} = recv_frame(socket)
    send_frame(socket, %{"t" => "provide", "name" => name})
    send_frame(socket, %{"t" => "rep", "req" => req, "result" => "ok"})
  end

  defp wait_until(fun, timeout \\ 3_000) do
    deadline = System.monotonic_time(:millisecond) + timeout
    poll(fun, deadline)
  end

  defp poll(fun, deadline) do
    cond do
      fun.() ->
        :ok

      System.monotonic_time(:millisecond) > deadline ->
        flunk("wait_until timed out")

      true ->
        Process.sleep(5)
        poll(fun, deadline)
    end
  end

  test "svc reaches a connecting client through the gateway", %{ctx: ctx, path: path} do
    socket = connect(path)
    handshake(socket, "echo")
    wait_until(fn -> :tenon.get(ctx, :echo) != :undefined end)

    task = Task.async(fn -> :tenon.svc(ctx, :echo, :ping, []) end)
    {:ok, %{"t" => "svc", "req" => svc_req, "method" => "ping"}} = recv_frame(socket)
    send_frame(socket, %{"t" => "rep", "req" => svc_req, "result" => "pong"})
    assert Task.await(task) == "pong"
  end

  test "disconnecting fails the client's fiber and drops its service", %{ctx: ctx, path: path} do
    socket = connect(path)
    handshake(socket, "echo")
    wait_until(fn -> :tenon.get(ctx, :echo) != :undefined end)

    :gen_tcp.close(socket)
    wait_until(fn -> :tenon.get(ctx, :echo) == :undefined end)
  end

  test "a second client gets its own fiber", %{ctx: ctx, path: path} do
    first = connect(path)
    handshake(first, "one")
    wait_until(fn -> :tenon.get(ctx, :one) != :undefined end)

    second = connect(path)
    handshake(second, "two")
    wait_until(fn -> :tenon.get(ctx, :two) != :undefined end)

    assert {:tenon_wire, one_fiber, :one} = :tenon.get(ctx, :one)
    assert {:tenon_wire, two_fiber, :two} = :tenon.get(ctx, :two)
    assert one_fiber != two_fiber
  end

  test "unmounting the gateway drops every connection", %{ctx: ctx, fiber: fiber, path: path} do
    socket = connect(path)
    handshake(socket, "echo")
    wait_until(fn -> :tenon.get(ctx, :echo) != :undefined end)

    assert :tenon.unmount(fiber) == :ok
    wait_until(fn -> :tenon.get(ctx, :echo) == :undefined end)
  end
end
