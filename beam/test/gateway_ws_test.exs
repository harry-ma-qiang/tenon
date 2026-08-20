defmodule Tenon.Beam.GatewayWsTest do
  use ExUnit.Case, async: false

  import Bitwise

  alias Tenon.Beam.Gateway

  @key "dGhlIHNhbXBsZSBub25jZQ=="

  setup do
    port = 21_000 + :rand.uniform(3_000)
    {:ok, kernel} = :tenon.start()
    ctx = :tenon.root(kernel)

    {:ok, _fiber} =
      :tenon.mount(ctx, %{
        module: Gateway,
        id: "gateway",
        config: %{address: "ws:127.0.0.1:#{port}"}
      })

    on_exit(fn -> :tenon.stop(kernel) end)
    %{ctx: ctx, port: port}
  end

  test "a ws client speaks hello/provide and its svc answers through the kernel", %{
    ctx: ctx,
    port: port
  } do
    socket = ws_connect(port)
    send_json(socket, %{"t" => "hello", "inject" => []})
    assert %{"t" => "load", "req" => req} = recv_json(socket)
    send_json(socket, %{"t" => "provide", "name" => "browser"})
    send_json(socket, %{"t" => "rep", "req" => req, "result" => "ok"})

    wait_until(fn -> :tenon.get(ctx, :browser) != :undefined end)

    task = Task.async(fn -> :tenon.svc(ctx, :browser, :click, [1]) end)
    assert %{"t" => "svc", "req" => svc_req, "method" => "click"} = recv_json(socket)
    send_json(socket, %{"t" => "rep", "req" => svc_req, "result" => "ok"})
    assert Task.await(task) == "ok"
  end

  test "closing the ws connection drops its fiber", %{ctx: ctx, port: port} do
    socket = ws_connect(port)
    send_json(socket, %{"t" => "hello", "inject" => []})
    assert %{"t" => "load", "req" => req} = recv_json(socket)
    send_json(socket, %{"t" => "provide", "name" => "browser"})
    send_json(socket, %{"t" => "rep", "req" => req, "result" => "ok"})
    wait_until(fn -> :tenon.get(ctx, :browser) != :undefined end)

    :gen_tcp.close(socket)
    wait_until(fn -> :tenon.get(ctx, :browser) == :undefined end)
  end

  defp ws_connect(port) do
    socket = connect(port)

    request =
      "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n" <>
        "Connection: Upgrade\r\nSec-WebSocket-Key: #{@key}\r\nSec-WebSocket-Version: 13\r\n\r\n"

    :ok = :gen_tcp.send(socket, request)
    assert read_101(socket, <<>>) =~ "101"
    socket
  end

  defp connect(port) do
    deadline = System.monotonic_time(:millisecond) + 3_000
    dial(port, deadline)
  end

  defp dial(port, deadline) do
    case :gen_tcp.connect(~c"127.0.0.1", port, [:binary, {:packet, 0}, active: false], 500) do
      {:ok, socket} ->
        socket

      {:error, _reason} ->
        if System.monotonic_time(:millisecond) > deadline, do: flunk("gateway never listened")
        Process.sleep(20)
        dial(port, deadline)
    end
  end

  defp read_101(socket, acc) do
    {:ok, data} = :gen_tcp.recv(socket, 0, 2_000)
    buffer = acc <> data
    if String.contains?(buffer, "\r\n\r\n"), do: buffer, else: read_101(socket, buffer)
  end

  defp send_json(socket, frame) do
    payload = Jason.encode!(frame)
    mask = :crypto.strong_rand_bytes(4)

    :gen_tcp.send(
      socket,
      header(byte_size(payload)) <> mask <> apply_mask(payload, mask, 0, <<>>)
    )
  end

  defp header(len) when len < 126, do: <<0x81, 0x80 ||| len>>
  defp header(len) when len < 65_536, do: <<0x81, 0x80 ||| 126, len::16>>
  defp header(len), do: <<0x81, 0x80 ||| 127, len::64>>

  defp apply_mask(<<>>, _mask, _index, acc), do: acc

  defp apply_mask(<<byte, rest::binary>>, mask, index, acc) do
    key = :binary.at(mask, rem(index, 4))
    apply_mask(rest, mask, index + 1, <<acc::binary, bxor(byte, key)>>)
  end

  defp recv_json(socket) do
    {:ok, <<_fin_op, len0>>} = :gen_tcp.recv(socket, 2, 2_000)
    len = extended_len(socket, len0)
    {:ok, payload} = :gen_tcp.recv(socket, len, 2_000)
    Jason.decode!(payload)
  end

  defp extended_len(socket, 126) do
    {:ok, <<len::16>>} = :gen_tcp.recv(socket, 2, 2_000)
    len
  end

  defp extended_len(socket, 127) do
    {:ok, <<len::64>>} = :gen_tcp.recv(socket, 8, 2_000)
    len
  end

  defp extended_len(_socket, len), do: len

  defp wait_until(fun, timeout \\ 3_000) do
    deadline = System.monotonic_time(:millisecond) + timeout
    poll(fun, deadline)
  end

  defp poll(fun, deadline) do
    cond do
      fun.() -> :ok
      System.monotonic_time(:millisecond) > deadline -> flunk("wait_until timed out")
      true -> Process.sleep(5) && poll(fun, deadline)
    end
  end
end
