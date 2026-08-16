defmodule Tenon.Events do
  @moduledoc """
  Hook table and the five dispatch modes.

  Dispatch always runs in the calling process and visits listeners in
  registration order; `prepend: true` puts a listener in front of the ones
  already registered.
  """

  require Logger

  alias Tenon.Ctx

  @task_timeout 5_000
  @max_arity 5

  @spec on(Ctx.t(), term(), function(), keyword()) :: Ctx.disposer()
  def on(ctx, event, fun, opts \\ []) when is_function(fun) do
    hooks = ctx.tables.hooks
    seq = ctx.tables.seq

    Ctx.effect(ctx, fn ->
      ref = make_ref()
      :ets.insert(hooks, {{event, order(seq, opts[:prepend])}, ref, ctx.fiber, fun})
      fn -> :ets.match_delete(hooks, {{event, :_}, ref, :_, :_}) end
    end)
  end

  @spec emit(Ctx.t(), term(), [term()]) :: :ok
  def emit(ctx, event, args) do
    Enum.each(hooks(ctx, event), fn fun -> isolate(event, fun, args) end)
  end

  @spec parallel(Ctx.t(), term(), [term()]) :: :ok | {:error, [term()]}
  def parallel(ctx, event, args) do
    errors =
      ctx
      |> hooks(event)
      |> Task.async_stream(fn fun -> isolate(event, fun, args) end,
        ordered: false,
        timeout: @task_timeout,
        on_timeout: :kill_task
      )
      |> Enum.flat_map(&failure/1)

    if errors == [], do: :ok, else: {:error, errors}
  end

  @spec serial(Ctx.t(), term(), [term()]) :: term()
  def serial(ctx, event, args), do: bail(ctx, event, args)

  @spec bail(Ctx.t(), term(), [term()]) :: term()
  def bail(ctx, event, args) do
    Enum.reduce_while(hooks(ctx, event), nil, fn fun, _acc ->
      case apply(fun, args) do
        result when result in [nil, false] -> {:cont, nil}
        result -> {:halt, result}
      end
    end)
  end

  @spec waterfall(Ctx.t(), term(), [term()], function()) :: term()
  def waterfall(ctx, event, args, terminal) when is_function(terminal) do
    arity = length(args)

    if arity > @max_arity do
      raise ArgumentError, "waterfall supports at most #{@max_arity} arguments"
    end

    chain = chain(hooks(ctx, event), terminal, arity)
    chain.(args)
  end

  @spec sweep(Ctx.tables(), pid()) :: :ok
  def sweep(tables, owner) do
    :ets.match_delete(tables.hooks, {{:_, :_}, :_, owner, :_})
    :ok
  end

  defp chain([], terminal, _arity), do: fn args -> apply(terminal, args) end

  defp chain([fun | rest], terminal, arity) do
    fn args -> apply(fun, args ++ [next(chain(rest, terminal, arity), arity)]) end
  end

  defp next(cont, 0), do: fn -> cont.([]) end
  defp next(cont, 1), do: fn a -> cont.([a]) end
  defp next(cont, 2), do: fn a, b -> cont.([a, b]) end
  defp next(cont, 3), do: fn a, b, c -> cont.([a, b, c]) end
  defp next(cont, 4), do: fn a, b, c, d -> cont.([a, b, c, d]) end
  defp next(cont, 5), do: fn a, b, c, d, e -> cont.([a, b, c, d, e]) end

  defp hooks(ctx, event) do
    ctx.tables.hooks
    |> :ets.match_object({{event, :_}, :_, :_, :_})
    |> Enum.map(&elem(&1, 3))
  end

  defp order(seq, true), do: :ets.update_counter(seq, :hook_prepend, -1)
  defp order(seq, _prepend), do: :ets.update_counter(seq, :hook_append, 1)

  defp failure({:ok, :ok}), do: []
  defp failure({:ok, {:error, reason}}), do: [reason]
  defp failure({:exit, reason}), do: [reason]

  defp isolate(event, fun, args) do
    apply(fun, args)
    :ok
  rescue
    exception ->
      Logger.error("tenon: listener for #{inspect(event)} raised #{Exception.message(exception)}")
      {:error, exception}
  catch
    kind, reason ->
      Logger.error("tenon: listener for #{inspect(event)} #{kind} #{inspect(reason)}")
      {:error, {kind, reason}}
  end
end
