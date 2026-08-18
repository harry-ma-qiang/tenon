defmodule Tenon.Beam.Gateway.Server do
  @moduledoc """
  Owns the listen socket and the acceptor loop; mounts one fiber per connection.
  """

  use GenServer

  require Logger

  @spec start_link(map(), String.t()) :: {:ok, pid()} | {:error, term()}
  def start_link(ctx, address) do
    with {:ok, listen} <- listen(address) do
      GenServer.start_link(__MODULE__, {ctx, listen, address})
    end
  end

  @spec stop(pid()) :: :ok
  def stop(pid), do: GenServer.stop(pid, :normal)

  @impl GenServer
  def init({ctx, listen, address}) do
    Process.flag(:trap_exit, true)
    Logger.info("tenon gateway: listening on #{address}")
    acceptor = spawn_link(fn -> accept_loop(ctx, listen, self()) end)
    {:ok, %{listen: listen, address: address, acceptor: acceptor, fibers: %{}}}
  end

  @impl GenServer
  def handle_cast({:connected, id, fiber}, state) do
    Process.monitor(fiber)
    Logger.info("tenon gateway: #{id} connected")
    {:noreply, put_in(state.fibers[fiber], id)}
  end

  @impl GenServer
  def handle_info({:DOWN, _ref, :process, pid, _reason}, state) do
    case Map.pop(state.fibers, pid) do
      {nil, fibers} ->
        {:noreply, %{state | fibers: fibers}}

      {id, fibers} ->
        Logger.info("tenon gateway: #{id} disconnected")
        {:noreply, %{state | fibers: fibers}}
    end
  end

  def handle_info({:EXIT, pid, :normal}, %{acceptor: pid} = state), do: {:noreply, state}

  def handle_info({:EXIT, pid, reason}, %{acceptor: pid} = state) do
    Logger.error("tenon gateway: acceptor on #{state.address} died: #{inspect(reason)}")
    {:noreply, state}
  end

  def handle_info(_message, state), do: {:noreply, state}

  @impl GenServer
  def terminate(_reason, state) do
    :gen_tcp.close(state.listen)
    :ok
  end

  defp listen("unix:" <> path) do
    File.rm(path)
    File.mkdir_p!(Path.dirname(path))

    :gen_tcp.listen(0, [
      :binary,
      {:packet, 4},
      {:ifaddr, {:local, to_charlist(path)}},
      active: false
    ])
  end

  defp listen("tcp:" <> host_port) do
    with [host, port_s] <- String.split(host_port, ":"),
         {port, ""} <- Integer.parse(port_s),
         {:ok, ip} <- :inet.parse_address(String.to_charlist(host)) do
      :gen_tcp.listen(port, [:binary, {:packet, 4}, {:ip, ip}, active: false, reuseaddr: true])
    else
      _other -> {:error, {:bad_address, host_port}}
    end
  end

  defp listen(address), do: {:error, {:bad_address, address}}

  defp accept_loop(ctx, listen, gateway) do
    case :gen_tcp.accept(listen) do
      {:ok, socket} ->
        claim(socket, ctx, gateway)
        accept_loop(ctx, listen, gateway)

      {:error, :closed} ->
        :ok

      {:error, reason} ->
        Logger.error("tenon gateway: accept failed: #{inspect(reason)}")
        accept_loop(ctx, listen, gateway)
    end
  end

  defp claim(socket, ctx, gateway) do
    pid = spawn(fn -> register(ctx, gateway) end)
    :ok = :gen_tcp.controlling_process(socket, pid)
    send(pid, {:socket, socket})
  end

  defp register(ctx, gateway) do
    receive do
      {:socket, socket} -> mount(ctx, gateway, socket)
    end
  end

  defp mount(ctx, gateway, socket) do
    id = "gw-#{System.unique_integer([:positive])}"

    case :tenon.mount(ctx, %{socket: socket, id: id}) do
      {:ok, fiber} -> GenServer.cast(gateway, {:connected, id, fiber})
      {:error, reason} -> Logger.error("tenon gateway: mount #{id} failed: #{inspect(reason)}")
    end
  end
end
