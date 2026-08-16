defmodule Tenon.Service do
  @moduledoc """
  Service registry ops and the `use Tenon.Service, name: :thing` plugin sugar.

  A provided service is an effect of the providing fiber: unloading the fiber
  withdraws the service and every dependent fiber unloads with it.
  """

  alias Tenon.Ctx
  alias Tenon.Events

  @callback start(Ctx.t(), term()) :: {:ok, term()} | {:error, term()}

  @doc "Turns a module into a plugin whose `start/2` result is provided under `name`."
  defmacro __using__(opts) do
    name = Keyword.fetch!(opts, :name)

    quote do
      use Tenon.Plugin

      @behaviour Tenon.Service

      @impl Tenon.Plugin
      def apply(ctx, config) do
        Tenon.Service.mount(ctx, unquote(name), start(ctx, config))
      end

      defoverridable apply: 2
    end
  end

  @doc "Provides the result of a `use Tenon.Service` plugin start."
  @spec mount(Ctx.t(), atom(), {:ok, term()} | {:error, term()}) :: :ok | {:error, term()}
  def mount(ctx, name, {:ok, impl}) do
    _disposer = provide(ctx, name, impl)
    :ok
  end

  def mount(_ctx, _name, {:error, reason}), do: {:error, reason}

  @doc "Publishes `impl` under `name` as an effect of the ctx fiber."
  @spec provide(Ctx.t(), atom(), term()) :: Ctx.disposer()
  def provide(ctx, name, impl) do
    Ctx.effect(ctx, fn -> install(ctx, name, impl) end)
  end

  defp install(ctx, name, impl) do
    if :ets.insert_new(ctx.tables.services, {name, impl, ctx.fiber}) do
      publish(ctx, name, impl)
      fn -> withdraw(ctx, name) end
    else
      raise ArgumentError, "service #{inspect(name)} is already provided"
    end
  end

  defp withdraw(ctx, name) do
    :ets.delete(ctx.tables.services, name)
    publish(ctx, name, nil)
  end

  defp publish(ctx, name, impl) do
    :ok = Tenon.Kernel.notify_services(ctx.kernel, [name])
    Events.emit(ctx, :"internal/service", [name, impl])
  end
end
