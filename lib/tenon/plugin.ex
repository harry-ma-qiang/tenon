defmodule Tenon.Plugin do
  @moduledoc """
  Behaviour implemented by every Tenon plugin.

  `apply/2` runs inside the owning fiber process once every injected service is
  available. It may return a disposer, which is registered as an effect.
  """

  @type config :: term()
  @type disposer :: (-> any())
  @type result :: :ok | {:ok, disposer()} | {:error, term()}

  @callback inject() :: [atom()]
  @callback apply(Tenon.Ctx.t(), config()) :: result()

  @optional_callbacks inject: 0

  @doc "Declares the behaviour and frees the `apply/2` name from `Kernel.apply/2`."
  defmacro __using__(_opts) do
    quote do
      @behaviour Tenon.Plugin

      import Kernel, except: [apply: 2]
    end
  end

  @doc "Service names a plugin module requires before it can load."
  @spec inject(module() | nil) :: [atom()]
  def inject(nil), do: []

  def inject(module) do
    Code.ensure_loaded!(module)

    if function_exported?(module, :inject, 0), do: module.inject(), else: []
  end
end
