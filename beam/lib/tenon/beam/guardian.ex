defmodule Tenon.Beam.Guardian do
  @moduledoc """
  The probe loop of the guardian node: it is the only plugin that watches another env.

  Every `interval` it runs one pass of `Tenon.Beam.Guardian.Probes` against `target`
  through the `link` service: env alive, kernel tree healthy, worker responsive, harness
  responsive, wedged waits, budgets, hard-rule violations in the log, plus whatever extra
  probes base approved. After `failures` consecutive passes with at least one failing
  probe it sends `reset{env, probes}` to base and starts counting again. Mounted only in
  the node whose `TENON_ROLE` is `guardian`.
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
