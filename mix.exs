defmodule Tenon.MixProject do
  use Mix.Project

  def project do
    [
      app: :tenon,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      elixirc_options: [warnings_as_errors: true],
      deps: deps()
    ]
  end

  def application do
    [
      extra_applications: [:logger],
      mod: {Tenon.Application, []}
    ]
  end

  defp deps do
    [
      {:credo, "~> 1.7", only: [:dev, :test], runtime: false},
      {:nimble_options, "~> 1.1"},
      {:jason, "~> 1.4"}
    ]
  end
end
