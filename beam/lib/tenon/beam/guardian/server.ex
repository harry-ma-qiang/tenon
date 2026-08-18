defmodule Tenon.Beam.Guardian.Server do
  @moduledoc "Timer, failure counter and reset trigger of `Tenon.Beam.Guardian`."

  use GenServer

  require Logger

  alias Tenon.Beam.Link.Handlers

  @interval 2_000
  @failures 6

  @spec start_link(map(), map()) :: {:ok, pid()} | {:error, term()}
  def start_link(ctx, config), do: GenServer.start_link(__MODULE__, {ctx, config})

  @impl GenServer
  def init({ctx, config}) do
    state = %{
      ctx: ctx,
      target: Handlers.opt(config, :target, "root"),
      interval: Handlers.opt(config, :interval, @interval),
      limit: Handlers.opt(config, :failures, @failures),
      notify: Handlers.opt(config, :notify, nil),
      strikes: 0
    }

    {:ok, arm(state)}
  end

  @impl GenServer
  def handle_info(:probe, state) do
    {:noreply, state |> score(probe(state)) |> arm()}
  end

  def handle_info(_message, state), do: {:noreply, state}

  defp arm(state) do
    Process.send_after(self(), :probe, state.interval)
    state
  end

  defp probe(state) do
    case call(state, "health", %{"env" => state.target}) do
      {:ok, %{"ok" => true}} -> :up
      other -> other
    end
  end

  defp score(state, :up) do
    tell(state, {:tenon_guardian, :up})
    %{state | strikes: 0}
  end

  defp score(state, reason) do
    strikes = state.strikes + 1
    Logger.warning("tenon guardian: #{state.target} unhealthy (#{strikes}) #{inspect(reason)}")
    tell(state, {:tenon_guardian, :strike, strikes})
    if strikes >= state.limit, do: reset(state), else: %{state | strikes: strikes}
  end

  defp reset(state) do
    Logger.error("tenon guardian: resetting #{state.target} after #{state.limit} failures")
    tell(state, {:tenon_guardian, :reset})
    call(state, "reset", %{"env" => state.target})
    %{state | strikes: 0}
  end

  defp call(state, method, params) do
    :tenon.svc(state.ctx, :link, :request, [method, params])
  catch
    kind, reason -> {:error, {kind, reason}}
  end

  defp tell(state, message) do
    if is_pid(state.notify), do: send(state.notify, message)
    :ok
  end
end
