defmodule Tenon.Beam.Check.Wire do
  @moduledoc """
  The wire half of the kernel contract: a socket-backed external fiber, the
  frame cap in both directions, and a hot swap of the module under a live
  kernel.

  The plugin is a process in this same VM speaking the wire over a loopback
  socket, so the suite needs no python, no script file and no writable
  directory on the machine it checks.
  """

  alias Tenon.Beam.Check.Plugins.Adder
  alias Tenon.Beam.Check.Plugins.Registrar
  alias Tenon.Beam.Check.Points

  @cap 4_096

  @spec names() :: [atom()]
  def names, do: [:socket_fiber, :frame_cap, :hot_swap]

  @spec run(atom(), {charlist(), binary()}) :: :ok | {:error, term()}
  def run(:socket_fiber, _beam), do: socket_fiber()
  def run(:frame_cap, _beam), do: frame_cap()
  def run(:hot_swap, beam), do: hot_swap(beam)

  defp socket_fiber do
    Points.with_kernel(fn ctx ->
      {fiber, plugin} = connect(ctx, %{})
      :active = :tenon.status(fiber)
      3 = :tenon.svc(ctx, :wiresvc, :add, [1, 2])
      "seen" = :tenon.svc(ctx, :wiresvc, :echo, ["seen"])
      2 = :tenon.call(ctx, :"wire/call", [1], fn value -> value end)
      :ok = :tenon.unmount(fiber)
      :ok = Points.until(fn -> not Process.alive?(plugin) end)
      :ok = Points.until(fn -> :tenon.get(ctx, :wiresvc) == :undefined end)
      :ok
    end)
  end

  defp frame_cap do
    Points.with_kernel(
      fn ctx ->
        {fiber, _plugin} = connect(ctx, %{})
        :active = :tenon.status(fiber)
        {:error, :frame_too_large} = :tenon.svc(ctx, :wiresvc, :big, [@cap * 4])
        {:error, :frame_too_large} = :tenon.svc(ctx, :wiresvc, :echo, [huge()])
        3 = :tenon.svc(ctx, :wiresvc, :add, [1, 2])
        :ok
      end,
      %{max_frame: @cap}
    )
  end

  defp huge, do: String.duplicate("x", @cap * 4)

  # The kernel process is not restarted by a code load and every shared row
  # lives in its ETS tables, so an active fiber, its service and its hooks must
  # all survive the swap.
  defp hot_swap({path, binary}) do
    Points.with_kernel(fn ctx ->
      {:ok, fiber} = :tenon.mount(ctx, %{module: Registrar, config: %{name: :kept, impl: Adder}})
      {socket, plugin} = connect(ctx, %{})
      :ok = reload(path, binary)
      :active = :tenon.status(fiber)
      :active = :tenon.status(socket)
      3 = :tenon.svc(ctx, :kept, :add, [1, 2])
      3 = :tenon.svc(ctx, :wiresvc, :add, [1, 2])
      :pong = :tenon.bail(ctx, :ping, [])
      :ok = :tenon.unmount(socket)
      :ok = Points.until(fn -> not Process.alive?(plugin) end)
      :ok
    end)
  end

  defp reload(path, binary) do
    :code.purge(:tenon)

    case :code.load_binary(:tenon, path, binary) do
      {:module, :tenon} -> :ok
      other -> {:error, "the module did not load again: #{inspect(other)}"}
    end
  end

  @doc """
  Accepts one loopback connection and mounts it as a socket fiber, with the
  plugin half running as a process of this VM.
  """
  @spec connect(map(), map()) :: {pid(), pid()}
  def connect(ctx, config) do
    {:ok, listen} = :gen_tcp.listen(0, [:binary, packet: 4, active: false, ip: {127, 0, 0, 1}])
    {:ok, port} = :inet.port(listen)
    parent = self()
    plugin = spawn(fn -> plugin(port, parent) end)

    receive do
      {:plugin_ready, ^plugin} -> :ok
    after
      3_000 -> exit(:plugin_never_connected)
    end

    {:ok, sock} = :gen_tcp.accept(listen, 3_000)
    :ok = :gen_tcp.close(listen)
    {:ok, fiber} = :tenon.mount(ctx, %{socket: sock, id: "check-wire", config: config})
    {fiber, plugin}
  end

  defp plugin(port, parent) do
    {:ok, sock} = :gen_tcp.connect({127, 0, 0, 1}, port, [:binary, packet: 4, active: false])
    send(parent, {:plugin_ready, self()})
    send_frame(sock, %{"t" => "hello", "inject" => []})
    loop(sock)
  end

  defp loop(sock) do
    case :gen_tcp.recv(sock, 0, 30_000) do
      {:ok, body} -> body |> Jason.decode!() |> answer(sock)
      {:error, _reason} -> :gen_tcp.close(sock)
    end
  end

  defp answer(%{"t" => "load", "req" => req}, sock) do
    send_frame(sock, %{"t" => "provide", "name" => "wiresvc"})
    send_frame(sock, hook_row())
    send_frame(sock, %{"t" => "rep", "req" => req, "result" => "ok"})
    loop(sock)
  end

  defp answer(%{"t" => "unload"}, sock), do: :gen_tcp.close(sock)

  defp answer(%{"t" => "hook", "req" => req, "args" => [value]}, sock) do
    send_frame(sock, %{"t" => "next", "req" => req, "args" => [value + 1]})
    loop(sock)
  end

  defp answer(%{"t" => "svc", "req" => req} = frame, sock) do
    send_frame(sock, %{"t" => "rep", "req" => req, "result" => result(frame)})
    loop(sock)
  end

  defp answer(_frame, sock), do: loop(sock)

  defp result(%{"method" => "add", "args" => [a, b]}), do: a + b
  defp result(%{"method" => "echo", "args" => [value]}), do: value
  defp result(%{"method" => "big", "args" => [size]}), do: String.duplicate("x", size)
  defp result(frame), do: "unknown #{Map.get(frame, "method")}"

  defp hook_row do
    %{"t" => "on", "hook" => 1, "event" => "wire/call", "arity" => 1, "mode" => "call"}
  end

  defp send_frame(sock, frame), do: :gen_tcp.send(sock, Jason.encode!(frame))
end
