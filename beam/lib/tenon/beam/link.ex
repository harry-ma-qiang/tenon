defmodule Tenon.Beam.Link do
  @moduledoc """
  The node half of the base link: one outbound UDS connection to the `tenon` base.

  Mounted in every node. On load it connects to `TENON_BASE_SOCK`, announces itself with
  `node.register`, answers the `health`, `tree` and `reload` requests base sends, and
  publishes the `link` service so a plugin in the same node can call base. When the
  connection closes the node stops: that is how `kill -9` of base takes every node down.
  """

  alias Tenon.Beam.Link.Server

  @spec inject() :: []
  def inject, do: []

  @spec load(map(), map()) :: {:ok, (-> :ok)} | {:error, term()}
  def load(ctx, config) do
    case Server.start_link(ctx, config) do
      {:ok, pid} ->
        :tenon.provide(ctx, :link, fn method, args -> Server.service(pid, method, args) end)
        {:ok, fn -> Server.stop(pid) end}

      {:error, reason} ->
        {:error, reason}
    end
  end

  @spec request(pid(), String.t(), map()) :: {:ok, term()} | {:error, term()}
  def request(pid, method, params), do: Server.service(pid, :request, [method, params])
end
