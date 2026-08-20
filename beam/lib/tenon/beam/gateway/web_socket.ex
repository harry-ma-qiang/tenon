defmodule Tenon.Beam.Gateway.WebSocket do
  @moduledoc """
  The WebSocket carrier of the gateway (RFC P4.4): a browser or extension speaks
  the same plugin wire, one JSON frame per WS text message, and each connection
  becomes a kernel socket-fiber exactly like a `tcp:`/`unix:` one.

  The kernel is frozen and mounts a real `{packet, 4}` socket, so this process
  bridges: it owns the browser socket and does WS framing, and it holds one end
  of a loopback socketpair whose other end is mounted. Kernel bytes come back
  length-prefixed on the pair and go out as WS text; WS text goes in on the pair
  and the kernel reads it as a plugin frame. Disconnect closes the pair, the
  fiber's socket closes, and the fiber is gone.
  """

  import Bitwise

  require Logger

  @magic "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
  @loopback {127, 0, 0, 1}

  @spec serve(:gen_tcp.socket(), map(), pid()) :: :ok
  def serve(socket, ctx, gateway) do
    with {:ok, key} <- handshake(socket, <<>>),
         :ok <- accept(socket, key),
         {:ok, inner, outer} <- socketpair() do
      # The kernel's mount blocks until the plugin's hello/load/rep handshake
      # completes, so the bridge must relay concurrently. It owns the browser and
      # outer sockets; this process keeps inner only long enough to mount it.
      bridge = spawn(fn -> await(socket, outer) end)
      :ok = :gen_tcp.controlling_process(socket, bridge)
      :ok = :gen_tcp.controlling_process(outer, bridge)
      send(bridge, :go)
      finish(mount(ctx, inner), gateway, bridge)
    else
      other -> fail(socket, other)
    end
  end

  defp finish({:ok, fiber, id}, gateway, _bridge) do
    GenServer.cast(gateway, {:connected, id, fiber})
    :ok
  end

  defp finish(error, _gateway, bridge) do
    send(bridge, :abort)
    Logger.error("tenon gateway ws: #{inspect(error)}")
    :ok
  end

  defp fail(socket, reason) do
    Logger.error("tenon gateway ws: #{inspect(reason)}")
    :gen_tcp.close(socket)
    :ok
  end

  defp await(browser, outer) do
    receive do
      :go ->
        :inet.setopts(browser, active: true, packet: 0)
        :inet.setopts(outer, active: true, packet: 4)
        loop(browser, outer, <<>>)

      :abort ->
        shutdown(browser, outer)
    end
  end

  defp handshake(socket, acc) do
    case :gen_tcp.recv(socket, 0, 5000) do
      {:ok, data} ->
        buffer = acc <> data

        if String.contains?(buffer, "\r\n\r\n"),
          do: extract_key(buffer),
          else: handshake(socket, buffer)

      {:error, reason} ->
        {:error, {:handshake, reason}}
    end
  end

  defp extract_key(request) do
    request
    |> String.split("\r\n")
    |> Enum.find_value(:missing_key, &key_of/1)
  end

  defp key_of(line) do
    case String.split(line, ":", parts: 2) do
      [name, value] -> key_pair(String.trim(name), String.trim(value))
      _other -> nil
    end
  end

  defp key_pair(name, value) do
    if String.downcase(name) == "sec-websocket-key", do: {:ok, value}, else: nil
  end

  defp accept(socket, key) do
    digest = :crypto.hash(:sha, key <> @magic) |> Base.encode64()

    response =
      "HTTP/1.1 101 Switching Protocols\r\n" <>
        "Upgrade: websocket\r\nConnection: Upgrade\r\n" <>
        "Sec-WebSocket-Accept: #{digest}\r\n\r\n"

    :gen_tcp.send(socket, response)
  end

  defp socketpair do
    opts = [:binary, {:packet, 4}, active: false]

    with {:ok, listen} <-
           :gen_tcp.listen(0, [{:ip, @loopback}, {:reuseaddr, true} | opts]),
         {:ok, port} <- :inet.port(listen),
         {:ok, outer} <- :gen_tcp.connect(@loopback, port, opts),
         {:ok, inner} <- :gen_tcp.accept(listen, 5000) do
      :gen_tcp.close(listen)
      {:ok, inner, outer}
    end
  end

  defp mount(ctx, inner) do
    id = "ws-#{System.unique_integer([:positive])}"

    case :tenon.mount(ctx, %{socket: inner, id: id}) do
      {:ok, fiber} -> {:ok, fiber, id}
      {:error, reason} -> {:error, {:mount, reason}}
    end
  end

  defp loop(browser, outer, buffer) do
    receive do
      {:tcp, ^browser, data} ->
        {frames, rest} = decode(buffer <> data, [])
        deliver(frames, browser, outer)
        if closing?(frames), do: shutdown(browser, outer), else: loop(browser, outer, rest)

      {:tcp, ^outer, payload} ->
        :gen_tcp.send(browser, encode(0x1, payload))
        loop(browser, outer, buffer)

      {:tcp_closed, _socket} ->
        shutdown(browser, outer)

      {:tcp_error, _socket, _reason} ->
        shutdown(browser, outer)
    end
  end

  defp shutdown(browser, outer) do
    :gen_tcp.close(outer)
    :gen_tcp.close(browser)
    :ok
  end

  defp deliver(frames, browser, outer) do
    Enum.each(frames, fn
      {0x1, payload} -> :gen_tcp.send(outer, payload)
      {0x9, payload} -> :gen_tcp.send(browser, encode(0xA, payload))
      _other -> :ok
    end)
  end

  defp closing?(frames), do: Enum.any?(frames, fn {op, _payload} -> op == 0x8 end)

  defp decode(buffer, acc) do
    case frame(buffer) do
      {:ok, op, payload, rest} -> decode(rest, [{op, payload} | acc])
      :incomplete -> {Enum.reverse(acc), buffer}
    end
  end

  defp frame(<<fin_op, mask_len, rest::binary>>) do
    op = fin_op &&& 0x0F
    masked = (mask_len &&& 0x80) != 0
    payload_len(op, masked, mask_len &&& 0x7F, rest)
  end

  defp frame(_buffer), do: :incomplete

  defp payload_len(op, masked, 126, <<len::16, rest::binary>>), do: body(op, masked, len, rest)
  defp payload_len(op, masked, 127, <<len::64, rest::binary>>), do: body(op, masked, len, rest)
  defp payload_len(_op, _masked, marker, _rest) when marker in [126, 127], do: :incomplete
  defp payload_len(op, masked, len, rest), do: body(op, masked, len, rest)

  defp body(op, masked, len, rest) do
    need = if masked, do: len + 4, else: len

    if byte_size(rest) < need do
      :incomplete
    else
      <<chunk::binary-size(need), tail::binary>> = rest
      {:ok, op, payload(masked, chunk), tail}
    end
  end

  defp payload(false, chunk), do: chunk

  defp payload(true, <<mask::binary-size(4), data::binary>>), do: unmask(data, mask)

  defp unmask(payload, mask), do: unmask(payload, mask, 0, <<>>)
  defp unmask(<<>>, _mask, _index, acc), do: acc

  defp unmask(<<byte, rest::binary>>, mask, index, acc) do
    key = :binary.at(mask, rem(index, 4))
    unmask(rest, mask, index + 1, <<acc::binary, bxor(byte, key)>>)
  end

  defp encode(op, payload) do
    len = byte_size(payload)

    header =
      cond do
        len < 126 -> <<0x80 ||| op, len>>
        len < 65_536 -> <<0x80 ||| op, 126, len::16>>
        true -> <<0x80 ||| op, 127, len::64>>
      end

    header <> payload
  end
end
