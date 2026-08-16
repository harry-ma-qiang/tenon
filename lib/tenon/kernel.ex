defmodule Tenon.Kernel do
  @moduledoc """
  One microkernel instance: ETS tables, the fiber supervisor and the registry sweep.

  The kernel never runs plugin code, never dispatches events and never calls a
  fiber synchronously; fibers call the kernel, the kernel casts to fibers.
  """

  use GenServer

  alias Tenon.Ctx
  alias Tenon.Events

  defstruct [:tables, :sup, :root, monitors: %{}]

  @spec start_link(keyword()) :: GenServer.on_start()
  def start_link(opts \\ []) do
    {name, opts} = Keyword.pop(opts, :name)

    case name do
      nil -> GenServer.start_link(__MODULE__, opts)
      name -> GenServer.start_link(__MODULE__, opts, name: name)
    end
  end

  @doc "Ctx of the root fiber; every top level plugin is mounted through it."
  @spec root(GenServer.server()) :: Ctx.t()
  def root(kernel), do: GenServer.call(kernel, :root)

  @doc "Nested snapshot of every live fiber, read from the status rows."
  @spec tree(GenServer.server()) :: map() | nil
  def tree(kernel), do: GenServer.call(kernel, :tree)

  @doc "Starts a fiber. Called by `Tenon.Ctx.plugin/4`, never by the kernel itself."
  @spec start_fiber(GenServer.server(), module() | nil, term(), keyword()) :: {:ok, pid()}
  def start_fiber(kernel, module, config, opts \\ []) do
    GenServer.call(kernel, {:start_fiber, module, config, opts})
  end

  @doc "Re-evaluates every fiber that injects one of `names`."
  @spec notify_services(GenServer.server(), [atom()]) :: :ok
  def notify_services(kernel, names), do: GenServer.call(kernel, {:notify_services, names})

  @impl GenServer
  def init(_opts) do
    tables = %{
      fibers: :ets.new(:fibers, [:set, :public, read_concurrency: true]),
      services: :ets.new(:services, [:set, :public, read_concurrency: true]),
      hooks: :ets.new(:hooks, [:ordered_set, :public, read_concurrency: true]),
      seq: :ets.new(:seq, [:set, :public, write_concurrency: true])
    }

    :ets.insert(tables.seq, [{:uid, 0}, {:hook_append, 0}, {:hook_prepend, 0}])
    {:ok, sup} = DynamicSupervisor.start_link(strategy: :one_for_one)
    state = %__MODULE__{tables: tables, sup: sup}
    {:ok, root, state} = spawn_fiber(state, nil, nil, [])
    {:ok, %{state | root: root}}
  end

  @impl GenServer
  def handle_call(:root, _from, state) do
    {:reply, %Ctx{kernel: self(), tables: state.tables, fiber: state.root, parent: nil}, state}
  end

  def handle_call(:tree, _from, state) do
    {:reply, build_tree(state), state}
  end

  def handle_call({:start_fiber, module, config, opts}, _from, state) do
    {:ok, pid, state} = spawn_fiber(state, module, config, opts)
    {:reply, {:ok, pid}, state}
  end

  def handle_call({:notify_services, names}, _from, state) do
    notify(state, names)
    {:reply, :ok, state}
  end

  @impl GenServer
  def handle_info({:DOWN, ref, :process, pid, _reason}, state) do
    names = sweep(state.tables, pid)
    dispose_children(state, pid)
    notify(state, names)
    {:noreply, %{state | monitors: Map.delete(state.monitors, ref)}}
  end

  def handle_info(_message, state), do: {:noreply, state}

  defp spawn_fiber(state, module, config, opts) do
    args = %{
      kernel: self(),
      tables: state.tables,
      module: module,
      config: config,
      id: opts[:id],
      parent: opts[:parent]
    }

    spec = %{id: Tenon.Fiber, start: {Tenon.Fiber, :start_link, [args]}, restart: :temporary}
    {:ok, pid} = DynamicSupervisor.start_child(state.sup, spec)
    ref = Process.monitor(pid)
    {:ok, pid, %{state | monitors: Map.put(state.monitors, ref, pid)}}
  end

  defp sweep(tables, owner) do
    Events.sweep(tables, owner)
    names = tables.services |> :ets.match({:"$1", :_, owner}) |> List.flatten()
    Enum.each(names, fn name -> :ets.delete(tables.services, name) end)
    :ets.delete(tables.fibers, owner)
    names
  end

  defp dispose_children(state, parent) do
    state.tables.fibers
    |> :ets.match({:"$1", :_, :_, parent, :_, :_, :_, :_, :_})
    |> List.flatten()
    |> Enum.each(fn child -> GenServer.cast(child, :dispose) end)
  end

  defp notify(_state, []), do: :ok

  defp notify(state, names) do
    state.tables.fibers
    |> :ets.tab2list()
    |> Enum.filter(fn row -> depends_on?(row, names) end)
    |> Enum.each(fn row -> GenServer.cast(elem(row, 0), :refresh) end)
  end

  defp depends_on?(row, names), do: Enum.any?(elem(row, 6), fn name -> name in names end)

  defp build_tree(state) do
    rows = :ets.tab2list(state.tables.fibers)
    index = Enum.group_by(rows, fn row -> elem(row, 3) end)

    case Enum.find(rows, fn row -> elem(row, 0) == state.root end) do
      nil -> nil
      row -> node_map(row, index)
    end
  end

  defp node_map(row, index) do
    {pid, uid, id, parent, module, status, inject, epoch, error} = row

    children =
      index
      |> Map.get(pid, [])
      |> Enum.sort_by(fn child -> elem(child, 1) end)
      |> Enum.map(fn child -> node_map(child, index) end)

    %{
      pid: pid,
      uid: uid,
      id: id,
      parent: parent,
      module: module,
      status: status,
      inject: inject,
      epoch: epoch,
      error: error,
      children: children
    }
  end
end
