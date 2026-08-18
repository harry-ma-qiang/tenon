defmodule Tenon.Beam.Test.Probe do
  @moduledoc "A do-nothing plugin the `plugin` request tests mount and unmount."

  @spec load(map(), term()) :: :ok
  def load(_ctx, _config), do: :ok
end
