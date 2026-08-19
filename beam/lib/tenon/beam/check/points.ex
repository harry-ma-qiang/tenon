defmodule Tenon.Beam.Check.Points do
  @moduledoc """
  The in-VM half of the kernel contract: lifecycle, disposers, the kill sweep,
  dependency gating, hook dispatch and the service table.

  Every point is a zero-argument function returning `:ok` or `{:error, reason}`;
  `Tenon.Beam.Check` runs them against whatever `tenon` module was loaded.
  """

  alias Tenon.Beam.Check.Plugins.Adder
  alias Tenon.Beam.Check.Plugins.Consumer
  alias Tenon.Beam.Check.Plugins.Db
  alias Tenon.Beam.Check.Plugins.Echo
  alias Tenon.Beam.Check.Plugins.Registrar
  alias Tenon.Beam.Check.Plugins.Stack

  @api [
    {:start, 0},
    {:start, 1},
    {:start_link, 0},
    {:start_link, 1},
    {:stop, 1},
    {:root, 1},
    {:tree, 1},
    {:status, 1},
    {:mount, 2},
    {:unmount, 1},
    {:restart, 1},
    {:restart, 2},
    {:effect, 2},
    {:on, 3},
    {:on, 4},
    {:emit, 3},
    {:call, 4},
    {:bail, 3},
    {:provide, 3},
    {:get, 2},
    {:svc, 4}
  ]

  @spec names() :: [atom()]
  def names,
    do: [:exports, :mount_unmount, :disposers, :kill_sweep, :inject, :hooks, :provide_svc]

  @spec run(atom()) :: :ok | {:error, term()}
  def run(:exports), do: exports()
  def run(:mount_unmount), do: mount_unmount()
  def run(:disposers), do: disposers()
  def run(:kill_sweep), do: kill_sweep()
  def run(:inject), do: inject()
  def run(:hooks), do: hooks()
  def run(:provide_svc), do: provide_svc()

  @doc "A kernel with one live kernel process, stopped whatever the body does."
  @spec with_kernel((map() -> term()), map()) :: term()
  def with_kernel(body, opts \\ %{}) do
    {:ok, kernel} = :tenon.start(opts)

    try do
      body.(:tenon.root(kernel))
    after
      :tenon.stop(kernel)
    end
  end

  defp exports do
    have = :tenon.module_info(:exports)
    missing = Enum.reject(@api, &(&1 in have))

    case missing do
      [] -> :ok
      rows -> {:error, "the module does not export #{inspect(rows)}"}
    end
  end

  defp mount_unmount do
    with_kernel(fn ctx ->
      {:ok, fiber} = :tenon.mount(ctx, %{module: Echo, config: %{pid: self(), tag: :a}})
      :ok = expect({:loaded, :a})
      :active = :tenon.status(fiber)
      %{children: [%{pid: ^fiber}]} = :tenon.tree(ctx.kernel)
      :ok = :tenon.unmount(fiber)
      :ok = expect({:disposed, :a})
      false = Process.alive?(fiber)
      %{children: []} = :tenon.tree(ctx.kernel)
      :ok
    end)
  end

  defp disposers do
    with_kernel(fn ctx ->
      config = %{pid: self(), tags: [:one, :two, :three]}
      {:ok, fiber} = :tenon.mount(ctx, %{module: Stack, config: config})
      :ok = :tenon.unmount(fiber)

      case drain([]) do
        [:three, :two, :one] -> :ok
        other -> {:error, "disposers ran #{inspect(other)}, not in reverse order"}
      end
    end)
  end

  defp kill_sweep do
    with_kernel(fn ctx ->
      spec = %{module: Registrar, config: %{name: :swept, impl: Adder}}
      {:ok, fiber} = :tenon.mount(ctx, spec)
      :active = :tenon.status(fiber)
      Process.exit(fiber, :kill)
      :ok = until(fn -> not Process.alive?(fiber) end)
      :ok = until(fn -> rows(ctx, :fibers, {fiber, :_, :_, :_, :_, :_, :_, :_, :_}) == [] end)
      :ok = until(fn -> rows(ctx, :services, {:swept, :_, :_}) == [] end)
      :ok = until(fn -> rows(ctx, :hooks, {{:ping, :_}, :_, fiber, :_}) == [] end)
      :ok
    end)
  end

  defp inject do
    with_kernel(fn ctx ->
      {:ok, consumer} = :tenon.mount(ctx, %{module: Consumer, config: %{pid: self()}})
      :pending = :tenon.status(consumer)
      {:ok, db} = :tenon.mount(ctx, %{module: Db, config: %{impl: Adder}})
      :ok = expect({:consumer_loaded, Adder})
      :active = :tenon.status(consumer)
      :ok = :tenon.unmount(db)
      :ok = expect({:consumer_unloaded, :db})
      :pending = :tenon.status(consumer)
      :ok
    end)
  end

  defp hooks do
    with_kernel(fn ctx ->
      pid = self()
      :tenon.on(ctx, :order, fn -> send(pid, {:hook, :first}) end)
      :tenon.on(ctx, :order, fn -> raise("isolated") end)
      :tenon.on(ctx, :order, fn -> send(pid, {:hook, :second}) end)
      :tenon.on(ctx, :order, fn -> send(pid, {:hook, :head}) end, %{prepend: true})
      :ok = :tenon.emit(ctx, :order, [])
      [:head, :first, :second] = drain([])
      :ok = waterfall(ctx)
      :ok = bail(ctx)
      :ok
    end)
  end

  defp waterfall(ctx) do
    :tenon.on(ctx, :flow, fn value, next -> next.(value + 1) end)
    :tenon.on(ctx, :flow, fn value, next -> next.(value * 2) end)
    4 = :tenon.call(ctx, :flow, [1], fn value -> value end)
    disposer = :tenon.on(ctx, :flow, fn _value, _next -> :short end, %{prepend: true})
    :short = :tenon.call(ctx, :flow, [1], fn value -> value end)
    disposer.()
    4 = :tenon.call(ctx, :flow, [1], fn value -> value end)
    :ok
  end

  defp bail(ctx) do
    :tenon.on(ctx, :pick, fn -> :undefined end)
    :tenon.on(ctx, :pick, fn -> :taken end)
    :tenon.on(ctx, :pick, fn -> :later end)
    :taken = :tenon.bail(ctx, :pick, [])
    :ok
  end

  defp provide_svc do
    with_kernel(fn ctx ->
      {:ok, _fiber} = :tenon.mount(ctx, %{module: Registrar, config: %{name: :math, impl: Adder}})
      3 = :tenon.svc(ctx, :math, :add, [1, 2])
      :tenon.provide(ctx, :fun, fn method, args -> {method, args} end)
      {:echo, [1]} = :tenon.svc(ctx, :fun, :echo, [1])
      :undefined = :tenon.get(ctx, :nothing)
      duplicate(ctx)
    end)
  end

  defp duplicate(ctx) do
    :tenon.provide(ctx, :math, Adder)
    {:error, "a duplicate service name was accepted"}
  rescue
    _error -> :ok
  end

  @doc "Waits for one message the suite expects a plugin to have sent."
  @spec expect(term(), non_neg_integer()) :: :ok | {:error, term()}
  def expect(message, timeout \\ 3_000) do
    receive do
      ^message -> :ok
    after
      timeout -> {:error, "expected #{inspect(message)} and it never arrived"}
    end
  end

  @doc "Polls until the body is true, or fails with a reason."
  @spec until((-> boolean()), non_neg_integer()) :: :ok | {:error, term()}
  def until(body, timeout \\ 3_000) do
    deadline = System.monotonic_time(:millisecond) + timeout
    poll(body, deadline)
  end

  defp poll(body, deadline) do
    cond do
      body.() -> :ok
      System.monotonic_time(:millisecond) > deadline -> {:error, "condition never became true"}
      true -> sleep_then(body, deadline)
    end
  end

  defp sleep_then(body, deadline) do
    Process.sleep(5)
    poll(body, deadline)
  end

  defp drain(acc) do
    receive do
      {:hook, value} -> drain([value | acc])
      {:disposed, value} -> drain([value | acc])
    after
      50 -> Enum.reverse(acc)
    end
  end

  defp rows(ctx, table, pattern), do: :ets.match_object(Map.fetch!(ctx.tabs, table), pattern)
end
