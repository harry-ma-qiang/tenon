defmodule Tenon.Beam.Application do
  @moduledoc """
  Starts `Tenon.Beam.Boot` when the VM was launched as a Tenon node.

  A node is a Tenon node when `TENON_ROLE` is set; without it the application starts
  empty, which is what `mix test` and a plain `iex -S mix` want.
  """

  use Application

  alias Tenon.Beam.Boot

  @impl Application
  def start(_type, _args) do
    children = if Boot.node?(), do: [Boot], else: []
    Supervisor.start_link(children, strategy: :one_for_one, name: Tenon.Beam.Supervisor)
  end
end
