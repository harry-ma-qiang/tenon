defmodule TenonSdkTest.MixProject do
  use Mix.Project

  def project do
    [
      app: :tenon_sdk_test,
      version: "0.1.0",
      elixir: "~> 1.18",
      start_permanent: false,
      deps: deps()
    ]
  end

  def application do
    [extra_applications: [:logger]]
  end

  defp deps do
    [{:tenon, path: "../../kernel"}]
  end
end
