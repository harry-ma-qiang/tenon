defmodule Tenon.Beam.Link.Server do
  @moduledoc """
  Owns the socket to base: sends `node.register`, answers base requests, correlates the
  replies of requests this node made, and stops the node when the socket closes.
  """

  use GenServer

  require Logger

  alias Tenon.Beam.Frame
  alias Tenon.Beam.Link.Handlers

  @call_timeout 15_000
  @halt_after 2_000

  @spec start_link(map(), map()) :: {:ok, pid()} | {:error, term()}
  def start_link(ctx, config) do
    with {:ok, socket} <- connect(Handlers.opt(config, :sock, nil)),
         {:ok, pid} <- GenServer.start_link(__MODULE__, {ctx, config, socket}),
         :ok <- :gen_tcp.controlling_process(socket, pid) do
      GenServer.call(pid, :activate)
      {:ok, pid}
    end
  end

  @spec stop(pid()) :: :ok
  def stop(pid), do: GenServer.stop(pid, :normal)

  @spec service(pid(), atom(), [term()]) :: term()
  def service(pid, :request, [method, params]),
    do: GenServer.call(pid, {:request, method, params}, @call_timeout + 1_000)

  def service(_pid, method, _args), do: {:error, {:unknown_method, method}}

  @impl GenServer
  def init({ctx, config, socket}) do
    {:ok,
     %{
       ctx: ctx,
       config: config,
       socket: socket,
       role: Handlers.opt(config, :role, "agent"),
       env: Handlers.opt(config, :env, "root"),
       halt: Handlers.opt(config, :halt, true),
       notify: Handlers.opt(config, :notify, nil),
       pending: %{},
       next: 1
     }}
  end

  @impl GenServer
  def handle_call(:activate, _from, state) do
    :ok = :inet.setopts(state.socket, active: true)

    send_frame(state, %{
      "t" => "node.register",
      "role" => state.role,
      "env" => state.env,
      "pid" => os_pid(),
      "token" => System.get_env("TENON_NODE_TOKEN", "")
    })

    {:reply, :ok, state}
  end

  def handle_call({:request, method, params}, from, state) do
    id = state.next

    frame =
      params
      |> Map.new(fn {k, v} -> {to_string(k), v} end)
      |> Map.merge(%{"t" => method, "id" => id})

    send_frame(state, frame)
    timer = Process.send_after(self(), {:expire, id}, @call_timeout)
    {:noreply, %{state | next: id + 1, pending: Map.put(state.pending, id, {from, timer})}}
  end

  @impl GenServer
  def handle_info({:tcp, _socket, body}, state) do
    case Frame.decode(body) do
      {:ok, frame} -> {:noreply, incoming(frame, state)}
      :error -> {:noreply, state}
    end
  end

  def handle_info({:tcp_closed, _socket}, state), do: down(:closed, state)
  def handle_info({:tcp_error, _socket, reason}, state), do: down(reason, state)

  def handle_info({:expire, id}, state) do
    {:noreply, answer(id, {:error, :timeout}, state)}
  end

  def handle_info(_message, state), do: {:noreply, state}

  defp incoming(%{"t" => "rep", "id" => id} = frame, state) do
    answer(id, Handlers.result(frame), state)
  end

  defp incoming(%{"t" => method, "id" => id}, state) when method in ["health", "tree"] do
    send_frame(state, %{"t" => "rep", "id" => id, "result" => Handlers.run(method, state)})
    state
  end

  defp incoming(%{"t" => "reload", "id" => id}, state) do
    send_frame(state, %{"t" => "rep", "id" => id, "result" => Handlers.run("reload", state)})
    state
  end

  defp incoming(%{"t" => "svc", "id" => id} = frame, state) do
    # Off the server process: a service call may be a whole model turn or a
    # minute of `bash`, and a health probe must not queue behind it.
    detach(state, id, fn ->
      case Handlers.svc(frame, state) do
        {:result, result} -> %{"result" => result}
        {:error, reason} -> %{"error" => reason}
      end
    end)

    state
  end

  defp incoming(%{"t" => "plugin", "id" => id} = frame, state) do
    detach(state, id, fn -> %{"result" => Handlers.plugin(frame, state)} end)
    state
  end

  defp incoming(%{"t" => other, "id" => id}, state) do
    send_frame(state, %{"t" => "rep", "id" => id, "error" => "unknown_method:#{other}"})
    state
  end

  defp incoming(_frame, state), do: state

  defp detach(state, id, body) do
    socket = state.socket

    spawn(fn ->
      frame = Map.merge(%{"t" => "rep", "id" => id}, outcome(body))
      :gen_tcp.send(socket, Frame.encode(frame))
    end)
  end

  # A detached body that raises would otherwise leave base waiting for a reply
  # that no process is going to send.
  defp outcome(body) do
    body.()
  rescue
    error -> %{"error" => Exception.message(error)}
  catch
    _kind, reason -> %{"error" => Frame.jsonable(reason)}
  end

  defp answer(id, result, state) do
    case Map.pop(state.pending, id) do
      {nil, _pending} ->
        state

      {{from, timer}, pending} ->
        Process.cancel_timer(timer)
        GenServer.reply(from, result)
        %{state | pending: pending}
    end
  end

  defp down(reason, state) do
    Logger.error("tenon link: base connection #{inspect(reason)}, stopping node #{state.env}")
    if is_pid(state.notify), do: send(state.notify, {:tenon_link, :down, reason})
    if state.halt, do: halt()
    {:stop, :normal, state}
  end

  defp halt do
    spawn(fn ->
      Process.sleep(@halt_after)
      System.halt(0)
    end)

    System.stop(0)
  end

  defp send_frame(state, frame), do: :gen_tcp.send(state.socket, Frame.encode(frame))

  defp connect(nil), do: {:error, :no_base_socket}

  defp connect(path) do
    :gen_tcp.connect({:local, to_charlist(path)}, 0, [:binary, {:packet, 4}, active: false])
  end

  defp os_pid, do: String.to_integer(System.pid())
end
