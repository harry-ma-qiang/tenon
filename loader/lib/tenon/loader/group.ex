defmodule Tenon.Loader.Group do
  @moduledoc "Fiber for a `group: true` row; its children are mounted under its ctx."

  @spec inject() :: []
  def inject, do: []

  @spec load(map(), term()) :: :ok
  def load(_ctx, _config), do: :ok
end
