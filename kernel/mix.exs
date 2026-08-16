defmodule Tenon.MixProject do
  use Mix.Project

  def project do
    [
      app: :tenon,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      erlc_options: [:warnings_as_errors, :debug_info],
      deps: []
    ]
  end

  def application do
    [extra_applications: [:logger]]
  end
end
