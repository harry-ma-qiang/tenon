defmodule Tenon.Beam.Guardian do
  @moduledoc """
  The probe loop of the guardian node: it is the only plugin that watches another env.

  Every `interval` it asks base for the health of `target` through the `link` service.
  After `failures` consecutive bad answers it sends `reset{env}` to base and starts
  counting again. Mounted only in the node whose `TENON_ROLE` is `guardian`.
  """

  alias Tenon.Beam.Guardian.Server

  @spec inject() :: [:link]
  def inject, do: [:link]

  @spec load(map(), map()) :: {:ok, (-> :ok)}
  def load(ctx, config) do
    {:ok, pid} = Server.start_link(ctx, config)
    {:ok, fn -> GenServer.stop(pid, :normal) end}
  end
end
