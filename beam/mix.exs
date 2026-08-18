defmodule Tenon.Beam.MixProject do
  use Mix.Project

  def project do
    [
      app: :tenon_beam,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: Mix.env() == :prod,
      elixirc_options: [warnings_as_errors: true],
      elixirc_paths: elixirc_paths(Mix.env()),
      releases: releases(),
      deps: deps()
    ]
  end

  def application do
    [extra_applications: [:logger], mod: {Tenon.Beam.Application, []}]
  end

  defp elixirc_paths(:test), do: ["lib", "test/support"]
  defp elixirc_paths(_env), do: ["lib"]

  defp releases do
    [
      tenon_beam: [
        include_executables_for: [:unix],
        include_erts: true,
        strip_beams: true,
        applications: [tenon_beam: :permanent]
      ]
    ]
  end

  defp deps do
    [
      {:tenon, path: "../kernel"},
      {:tenon_loader, path: "../loader"},
      {:jason, "~> 1.4"},
      {:yaml_elixir, "~> 2.11"},
      {:credo, "~> 1.7", only: [:dev, :test], runtime: false}
    ]
  end
end
