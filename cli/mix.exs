defmodule Tenon.CLI.MixProject do
  use Mix.Project

  def project do
    [
      app: :tenon_cli,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      elixirc_options: [warnings_as_errors: true],
      escript: [main_module: Tenon.CLI, name: :tenon],
      deps: deps()
    ]
  end

  def application do
    [extra_applications: [:logger]]
  end

  defp deps do
    [
      {:tenon, path: "../kernel"},
      {:tenon_loader, path: "../loader"},
      {:yaml_elixir, "~> 2.11"},
      {:credo, "~> 1.7", only: [:dev, :test], runtime: false}
    ]
  end
end
