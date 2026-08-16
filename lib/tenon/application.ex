defmodule Tenon.Application do
  @moduledoc false

  use Application

  @impl Application
  @spec start(Application.start_type(), term()) :: {:ok, pid()} | {:error, term()}
  def start(_type, _args) do
    children = []
    opts = [strategy: :one_for_one, name: Tenon.Supervisor]
    Supervisor.start_link(children, opts)
  end
end
