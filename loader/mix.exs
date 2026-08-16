defmodule Tenon.Loader.MixProject do
  use Mix.Project

  def project do
    [
      app: :tenon_loader,
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
      mod: {Tenon.Loader.Application, []}
    ]
  end

  defp deps do
    [
      {:tenon, path: "../kernel"},
      {:yaml_elixir, "~> 2.11"},
      {:jason, "~> 1.4"},
      {:credo, "~> 1.7", only: [:dev, :test], runtime: false}
    ]
  end
end
