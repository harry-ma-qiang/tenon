defmodule Tenon.Loader do
  @moduledoc """
  In-VM Tenon plugin that composes a Cordis/DSH config tree and mounts it.

  Config: `%{layers: [layer], registry: %{name => spec}, collapse: [{prefix, fun}], dsh: map()}`.
  See `README.md` for the row, patch and `!!js` semantics.
  """

  require Logger

  alias Tenon.Loader.Config
  alias Tenon.Loader.Server
  alias Tenon.Loader.Tree

  @spec inject() :: []
  def inject, do: []

  @spec load(map(), map()) :: {:ok, (-> :ok)}
  def load(ctx, config) do
    state = compose(ctx, config, Tree.empty())
    {:ok, pid} = Server.start(ctx, config, state)
    {:ok, fn -> Server.stop(pid) end}
  end

  @spec compose(map(), map(), Tree.state()) :: Tree.state()
  def compose(ctx, config, state) do
    {rows, warnings} = Config.compose(Map.get(config, :layers, []))
    Enum.each(warnings, &Logger.warning("tenon loader: " <> &1))

    built =
      rows
      |> Tree.build(config)
      |> Map.put(:warnings, warnings)

    Tree.sync(ctx, state, built)
  end

  @spec reload(pid()) :: :ok
  def reload(loader), do: Server.call(loader, :reload)

  @spec dump(pid()) :: [map()]
  def dump(loader), do: Server.call(loader, :dump)

  @spec warnings(pid()) :: [String.t()]
  def warnings(loader), do: Server.call(loader, :state).warnings
end
