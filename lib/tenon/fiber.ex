defmodule Tenon.Fiber do
  @moduledoc """
  One process per plugin instance.

  The fiber owns its effect stack and is the only writer of its status row. It
  loads when every injected service is present, unloads when one disappears and
  reloads when a provider is replaced.
  """

  use GenServer

  require Logger

  alias Tenon.Ctx
  alias Tenon.Events
  alias Tenon.Plugin

  @type status :: :pending | :loading | :active | :failed | :unloading | :disposed
  @type epoch :: :inactive | [{atom(), pid()}]

  defstruct [:ctx, :uid, :id, :module, :config, :inject, :epoch, :status, :error, disposers: []]

  @timeout 15_000

  @spec start_link(map()) :: GenServer.on_start()
  def start_link(args), do: GenServer.start_link(__MODULE__, args)

  @doc "Settles pending dependency changes and returns the current status."
  @spec status(pid()) :: status()
  def status(fiber), do: GenServer.call(fiber, :status, @timeout)

  @doc "Unloads the plugin and stops the fiber process."
  @spec dispose(pid()) :: :ok
  def dispose(fiber) do
    GenServer.call(fiber, :dispose, @timeout)
  catch
    :exit, _reason -> :ok
  end

  @doc "Replaces the config and reloads the plugin when it is loaded."
  @spec update(pid(), term()) :: :ok
  def update(fiber, config), do: GenServer.call(fiber, {:update, config}, @timeout)

  @doc "Unloads and loads the plugin again with its current config."
  @spec restart(pid()) :: :ok
  def restart(fiber), do: GenServer.call(fiber, :restart, @timeout)

  @doc "Hands a disposer to `fiber` and returns the disposer for that one effect."
  @spec register(pid(), Ctx.disposer()) :: Ctx.disposer()
  def register(fiber, disposer) when is_function(disposer, 0) do
    ref = make_ref()

    if self() == fiber do
      send(self(), {:tenon_effect, ref, disposer})
    else
      :ok = GenServer.call(fiber, {:effect, ref, disposer}, @timeout)
    end

    fn -> drop(fiber, ref) end
  end

  @impl GenServer
  def init(args) do
    %{kernel: kernel, tables: tables, module: module, config: config} = args
    uid = :ets.update_counter(tables.seq, :uid, 1)
    ctx = %Ctx{kernel: kernel, tables: tables, fiber: self(), parent: args.parent}

    state = %__MODULE__{
      ctx: ctx,
      uid: uid,
      id: args.id,
      module: module,
      config: config,
      inject: Plugin.inject(module),
      epoch: :inactive,
      status: :pending
    }

    write_row(state)
    {:ok, state, {:continue, :mount}}
  end

  @impl GenServer
  def handle_continue(:mount, state) do
    Events.emit(state.ctx, :"internal/plugin", [self()])
    {:noreply, refresh(state)}
  end

  @impl GenServer
  def handle_call(:status, _from, state) do
    state = refresh(state)
    {:reply, state.status, state}
  end

  def handle_call(:dispose, _from, state) do
    {:stop, :normal, :ok, teardown(state)}
  end

  def handle_call(:restart, _from, state) do
    {:reply, :ok, reload(state)}
  end

  def handle_call({:update, config}, _from, state) do
    {:reply, :ok, reload(%{state | config: config})}
  end

  def handle_call({:effect, ref, disposer}, _from, state) do
    {:reply, :ok, %{state | disposers: [{ref, disposer} | state.disposers]}}
  end

  def handle_call({:drop, ref}, _from, state) do
    {:reply, :ok, run_disposer(state, ref)}
  end

  @impl GenServer
  def handle_cast(:refresh, state), do: {:noreply, refresh(state)}

  def handle_cast(:dispose, state), do: {:stop, :normal, teardown(state)}

  @impl GenServer
  def handle_info({:tenon_effect, ref, disposer}, state) do
    {:noreply, %{state | disposers: [{ref, disposer} | state.disposers]}}
  end

  def handle_info({:tenon_drop, ref}, state), do: {:noreply, run_disposer(state, ref)}

  def handle_info(_message, state), do: {:noreply, state}

  defp drop(fiber, ref) do
    if self() == fiber do
      send(self(), {:tenon_drop, ref})
      :ok
    else
      GenServer.call(fiber, {:drop, ref}, @timeout)
    end
  end

  defp teardown(state) do
    state = state |> unload() |> set_status(:disposed)
    :ets.delete(state.ctx.tables.fibers, self())
    state
  end

  defp refresh(%{status: :disposed} = state), do: state

  defp refresh(state) do
    epoch = compute_epoch(state)

    cond do
      epoch == state.epoch ->
        state

      state.epoch == :inactive ->
        load(%{state | epoch: epoch})

      epoch == :inactive ->
        state |> unload() |> Map.put(:epoch, :inactive) |> set_status(:pending)

      true ->
        state |> unload() |> Map.put(:epoch, epoch) |> load()
    end
  end

  defp reload(%{status: :disposed} = state), do: state

  defp reload(state) do
    state
    |> unload()
    |> Map.put(:epoch, :inactive)
    |> set_status(:pending)
    |> refresh()
  end

  defp load(state) do
    state = set_status(state, :loading)
    result = run_apply(state)
    state = drain(state)

    case result do
      :ok ->
        set_status(state, :active)

      {:ok, disposer} when is_function(disposer, 0) ->
        state = %{state | disposers: [{make_ref(), disposer} | state.disposers]}
        set_status(state, :active)

      {:error, reason} ->
        fail(state, reason)

      other ->
        fail(state, {:bad_return, other})
    end
  end

  defp fail(state, reason) do
    Logger.error("tenon: #{inspect(state.module)} failed to load: #{inspect(reason)}")
    set_status(%{state | error: reason}, :failed)
  end

  defp run_apply(%{module: nil}), do: :ok

  defp run_apply(state) do
    state.module.apply(state.ctx, state.config)
  rescue
    exception -> {:error, {exception, __STACKTRACE__}}
  catch
    kind, reason -> {:error, {kind, reason}}
  end

  defp unload(state) do
    state = set_status(state, :unloading)
    disposers = state.disposers
    state = %{state | disposers: [], error: nil}
    Enum.each(disposers, fn {_ref, disposer} -> run(disposer) end)
    sweep(state)
    drain(state)
  end

  defp sweep(state) do
    tables = state.ctx.tables
    Events.sweep(tables, self())
    names = tables.services |> :ets.match({:"$1", :_, self()}) |> List.flatten()
    Enum.each(names, fn name -> :ets.delete(tables.services, name) end)

    if names != [] do
      :ok = Tenon.Kernel.notify_services(state.ctx.kernel, names)
    end

    :ok
  end

  defp drain(state) do
    receive do
      {:tenon_effect, ref, disposer} ->
        drain(%{state | disposers: [{ref, disposer} | state.disposers]})

      {:tenon_drop, ref} ->
        drain(run_disposer(state, ref))
    after
      0 -> state
    end
  end

  defp run_disposer(state, ref) do
    case List.keytake(state.disposers, ref, 0) do
      {{^ref, disposer}, rest} ->
        run(disposer)
        %{state | disposers: rest}

      nil ->
        state
    end
  end

  defp run(disposer) do
    disposer.()
    :ok
  rescue
    exception -> Logger.error("tenon: disposer raised #{Exception.message(exception)}")
  catch
    kind, reason -> Logger.error("tenon: disposer #{kind} #{inspect(reason)}")
  end

  defp set_status(%{status: status} = state, status), do: state

  defp set_status(state, status) do
    old = state.status
    state = %{state | status: status}
    write_row(state)
    Events.emit(state.ctx, :"internal/status", [self(), old, status])
    state
  end

  defp write_row(state) do
    :ets.insert(
      state.ctx.tables.fibers,
      {self(), state.uid, state.id, state.ctx.parent, state.module, state.status, state.inject,
       state.epoch, state.error}
    )
  end

  defp compute_epoch(state), do: collect(state.inject, state.ctx.tables.services, [])

  defp collect([], _services, acc), do: Enum.reverse(acc)

  defp collect([name | rest], services, acc) do
    case :ets.lookup(services, name) do
      [{^name, _impl, owner}] -> collect(rest, services, [{name, owner} | acc])
      [] -> :inactive
    end
  end
end
