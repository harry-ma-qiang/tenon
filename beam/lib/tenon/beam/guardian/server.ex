defmodule Tenon.Beam.Guardian.Server do
  @moduledoc "Timer, probe pass, failure counter and reset trigger of `Tenon.Beam.Guardian`."

  use GenServer

  require Logger

  alias Tenon.Beam.Guardian.Probes
  alias Tenon.Beam.Link.Handlers

  @interval 2_000
  @failures 6
  @probe_timeout 5_000

  @spec start_link(map(), map()) :: {:ok, pid()} | {:error, term()}
  def start_link(ctx, config), do: GenServer.start_link(__MODULE__, {ctx, config})

  @impl GenServer
  def init({ctx, config}) do
    state = %{
      ctx: ctx,
      target: Handlers.opt(config, :target, "root"),
      interval: Handlers.opt(config, :interval, @interval),
      limit: Handlers.opt(config, :failures, @failures),
      probe_timeout: Handlers.opt(config, :probe_timeout, @probe_timeout),
      probes: Handlers.opt(config, :probes, []),
      notify: Handlers.opt(config, :notify, nil),
      after: 0,
      status: nil,
      strikes: 0
    }

    Logger.info("tenon guardian: #{length(state.probes)} extra probes for #{state.target}")
    {:ok, arm(state)}
  end

  @impl GenServer
  def handle_info(:probe, state) do
    {failed, state} = Probes.run(state)
    {:noreply, state |> score(failed) |> arm()}
  end

  def handle_info(_message, state), do: {:noreply, state}

  defp arm(state) do
    Process.send_after(self(), :probe, state.interval)
    state
  end

  defp score(state, []) do
    tell(state, {:tenon_guardian, :up})
    %{state | strikes: 0}
  end

  defp score(state, failed) do
    strikes = state.strikes + 1
    names = Enum.map(failed, &elem(&1, 0))
    Logger.warning("tenon guardian: #{state.target} failed #{Enum.join(names, ",")} (#{strikes})")
    tell(state, {:tenon_guardian, :failed, names})
    tell(state, {:tenon_guardian, :strike, strikes})
    if strikes >= state.limit, do: reset(state, names), else: %{state | strikes: strikes}
  end

  defp reset(state, names) do
    Logger.error("tenon guardian: resetting #{state.target} after #{state.limit} failures")
    tell(state, {:tenon_guardian, :reset})
    call(state, "reset", %{"env" => state.target, "probes" => names})
    %{state | strikes: 0}
  end

  defp call(state, method, params) do
    :tenon.svc(state.ctx, :link, :request, [method, params, state.probe_timeout])
  catch
    kind, reason -> {:error, {kind, reason}}
  end

  defp tell(state, message) do
    if is_pid(state.notify), do: send(state.notify, message)
    :ok
  end
end
