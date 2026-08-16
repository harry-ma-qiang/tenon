defmodule Tenon.Loader.Application do
  @moduledoc "Owns the registry that maps a loader fiber pid to its ops server."

  use Application

  @impl Application
  def start(_type, _args) do
    children = [{Registry, keys: :unique, name: Tenon.Loader.Registry}]
    Supervisor.start_link(children, strategy: :one_for_one, name: Tenon.Loader.Supervisor)
  end
end
