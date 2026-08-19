defmodule Tenon.Beam.Test.Base do
  @moduledoc false

  use GenServer

  alias Tenon.Beam.Frame

  @spec start(pid()) :: {pid(), Path.t()}
  def start(owner) do
    path = Path.join(System.tmp_dir!(), "tenon-base-#{System.unique_integer([:positive])}.sock")
    {:ok, pid} = GenServer.start(__MODULE__, {owner, path})
    {pid, path}
  end

  @spec answer(
          pid(),
          String.t() | {String.t(), String.t()},
          {:ok, term()} | {:error, term()} | :ignore
        ) :: :ok
  def answer(pid, method, reply), do: GenServer.call(pid, {:answer, method, reply})

  @spec push(pid(), map()) :: :ok
  def push(pid, frame), do: GenServer.call(pid, {:push, frame})

  @spec close(pid()) :: :ok
  def close(pid), do: GenServer.call(pid, :close)

  @spec shutdown(pid()) :: :ok
  def shutdown(pid), do: GenServer.stop(pid, :normal)

  @impl GenServer
  def init({owner, path}) do
    File.rm(path)
    opts = [:binary, {:packet, 4}, {:ifaddr, {:local, to_charlist(path)}}, active: false]
    {:ok, listen} = :gen_tcp.listen(0, opts)
    server = self()
    spawn_link(fn -> accept(listen, server) end)
    {:ok, %{owner: owner, path: path, listen: listen, socket: nil, answers: %{}}}
  end

  @impl GenServer
  def handle_call({:answer, method, reply}, _from, state),
    do: {:reply, :ok, put_in(state.answers[method], reply)}

  def handle_call({:push, frame}, _from, state),
    do: {:reply, :gen_tcp.send(state.socket, Frame.encode(frame)), state}

  def handle_call(:close, _from, state), do: {:reply, :gen_tcp.close(state.socket), state}

  @impl GenServer
  def handle_info({:accepted, socket}, state) do
    :ok = :inet.setopts(socket, active: true)
    {:noreply, %{state | socket: socket}}
  end

  def handle_info({:tcp, _socket, body}, state) do
    {:ok, frame} = Frame.decode(body)
    send(state.owner, {:base, frame})
    {:noreply, reply(frame, state)}
  end

  def handle_info(_message, state), do: {:noreply, state}

  @impl GenServer
  def terminate(_reason, state) do
    :gen_tcp.close(state.listen)
    File.rm(state.path)
    :ok
  end

  defp accept(listen, server) do
    {:ok, socket} = :gen_tcp.accept(listen, 5_000)
    :ok = :gen_tcp.controlling_process(socket, server)
    send(server, {:accepted, socket})
  end

  defp reply(%{"t" => method, "id" => id} = frame, state) do
    case fetch(state.answers, method, Map.get(frame, "name")) do
      {:ok, :ignore} -> state
      {:ok, {:ok, result}} -> push_now(state, %{"t" => "rep", "id" => id, "result" => result})
      {:ok, {:error, error}} -> push_now(state, %{"t" => "rep", "id" => id, "error" => error})
      :error -> state
    end
  end

  defp reply(_frame, state), do: state

  defp fetch(answers, method, nil), do: Map.fetch(answers, method)

  defp fetch(answers, method, name) do
    case Map.fetch(answers, {method, name}) do
      :error -> Map.fetch(answers, method)
      found -> found
    end
  end

  defp push_now(state, frame) do
    :gen_tcp.send(state.socket, Frame.encode(frame))
    state
  end
end
