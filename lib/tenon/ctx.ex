defmodule Tenon.Ctx do
  @moduledoc """
  Handle a plugin holds on its kernel: the facade for every kernel operation.

  A ctx names the owning fiber, so every registration made through it is an
  effect of that fiber and is undone when the fiber unloads.
  """

  alias Tenon.Events
  alias Tenon.Fiber
  alias Tenon.Service

  @enforce_keys [:kernel, :tables, :fiber]
  defstruct [:kernel, :tables, :fiber, :parent]

  @type tables :: %{
          fibers: :ets.tid(),
          services: :ets.tid(),
          hooks: :ets.tid(),
          seq: :ets.tid()
        }
  @type t :: %__MODULE__{kernel: pid(), tables: tables(), fiber: pid(), parent: pid() | nil}
  @type disposer :: (-> any())

  @doc "Mounts `module` as a child fiber of the ctx owner and waits for it to settle."
  @spec plugin(t(), module(), term(), keyword()) :: {:ok, pid()}
  def plugin(ctx, module, config \\ %{}, opts \\ []) do
    opts = Keyword.put(opts, :parent, ctx.fiber)
    {:ok, pid} = Tenon.Kernel.start_fiber(ctx.kernel, module, config, opts)
    Fiber.register(ctx.fiber, fn -> Fiber.dispose(pid) end)
    _ = Fiber.status(pid)
    {:ok, pid}
  end

  @doc """
  Runs `fun` now in the calling process and hands its disposer to the fiber.

  The returned disposer undoes only this effect; unloading the fiber runs every
  live disposer in reverse registration order.
  """
  @spec effect(t(), (-> disposer() | nil)) :: disposer()
  def effect(ctx, fun) do
    case fun.() do
      disposer when is_function(disposer, 0) ->
        Fiber.register(ctx.fiber, disposer)

      nil ->
        fn -> :ok end

      other ->
        raise ArgumentError, "effect must return a 0-arity disposer or nil, got #{inspect(other)}"
    end
  end

  @doc "Looks up a provided service, or nil when nobody provides it."
  @spec get(t(), atom()) :: term() | nil
  def get(ctx, name) do
    case :ets.lookup(ctx.tables.services, name) do
      [{^name, impl, _owner}] -> impl
      [] -> nil
    end
  end

  @doc "Registers an event listener owned by the ctx fiber. Options: `prepend: true`."
  @spec on(t(), term(), function(), keyword()) :: disposer()
  defdelegate on(ctx, event, fun, opts \\ []), to: Events

  @doc "Fires every listener in registration order, isolating listener failures."
  @spec emit(t(), term(), [term()]) :: :ok
  defdelegate emit(ctx, event, args), to: Events

  @doc "Runs every listener concurrently and collects failures."
  @spec parallel(t(), term(), [term()]) :: :ok | {:error, [term()]}
  defdelegate parallel(ctx, event, args), to: Events

  @doc "Runs listeners in order until one returns a value other than nil or false."
  @spec serial(t(), term(), [term()]) :: term()
  defdelegate serial(ctx, event, args), to: Events

  @doc "Same as `serial/3`; both dispatch synchronously on the BEAM."
  @spec bail(t(), term(), [term()]) :: term()
  defdelegate bail(ctx, event, args), to: Events

  @doc "Wraps `terminal` in the listener chain; a listener that skips `next` short-circuits."
  @spec waterfall(t(), term(), [term()], function()) :: term()
  defdelegate waterfall(ctx, event, args, terminal), to: Events

  @doc "Publishes a service under `name` as an effect of the ctx fiber."
  @spec provide(t(), atom(), term()) :: disposer()
  defdelegate provide(ctx, name, impl), to: Service
end
