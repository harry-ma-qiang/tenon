defmodule Tenon.Beam.Registry do
  @moduledoc """
  The `name => spec` map handed to `Tenon.Loader`: the two builtin names plus the rows of
  the `registry.yml` that sits next to the profile base wrote.
  """

  alias Tenon.Loader
  alias Tenon.Loader.Group

  @builtin %{"cordis:group" => %{module: Group}, "tenon:loader" => %{module: Loader}}

  @spec builtin() :: map()
  def builtin, do: @builtin

  @spec load(nil | Path.t()) :: map()
  def load(nil), do: @builtin

  def load(path) do
    if File.regular?(path), do: Map.merge(@builtin, read(path)), else: @builtin
  end

  defp read(path) do
    case YamlElixir.read_from_file(path) do
      {:ok, map} when is_map(map) -> Map.new(map, &entry/1)
      _other -> %{}
    end
  end

  defp entry({name, spec}), do: {to_string(name), spec(spec)}

  defp spec(%{"module" => module}), do: %{module: Module.concat([module])}

  defp spec(%{"cmd" => cmd} = spec) do
    %{
      cmd: cmd,
      args: Enum.map(Map.get(spec, "args", []), &to_string/1),
      env: env(Map.get(spec, "env", []))
    }
  end

  defp spec(other), do: %{module: Module.concat([inspect(other)])}

  defp env(pairs) do
    Enum.map(pairs, fn [name, value] ->
      {String.to_charlist(to_string(name)), String.to_charlist(to_string(value))}
    end)
  end
end
