defmodule Tenon.Beam.Gateway do
  @moduledoc """
  The in-sandbox registration path: a plugin that listens so other processes can mount.

  Mounted only in agent nodes. It listens on `config.address` (`unix:<path>` or
  `tcp:<host>:<port>`) and, for every accepted connection, calls `:tenon.mount/2` with
  `%{socket: sock}` under this plugin's own ctx, so the connection becomes a real fiber
  and unmounting the gateway drops every connection with it.
  """

  alias Tenon.Beam.Gateway.Server

  @spec inject() :: []
  def inject, do: []

  @spec load(map(), map()) :: {:ok, (-> :ok)} | {:error, term()}
  def load(ctx, config) do
    case Server.start_link(ctx, Map.fetch!(config, :address)) do
      {:ok, pid} -> {:ok, fn -> Server.stop(pid) end}
      {:error, reason} -> {:error, reason}
    end
  end
end
