defmodule Tenon.CLI.Registry do
  @moduledoc """
  The plugin name registry handed to `Tenon.Loader`: built-in names plus the entries of
  a `--registry` source (a `.yml` map, a `.exs` file returning a map, or a module
  exporting `registry/0`).
  """

  alias Tenon.Loader.Group

  @builtin %{
    "cordis:group" => %{module: Group},
    "tenon:loader" => %{module: Tenon.Loader}
  }

  @spec builtin() :: map()
  def builtin, do: @builtin

  @spec load(nil | String.t()) :: {:ok, map()} | {:error, String.t()}
  def load(nil), do: {:ok, @builtin}

  def load(source) do
    case read(source) do
      {:ok, extra} -> {:ok, Map.merge(@builtin, extra)}
      {:error, message} -> {:error, message}
    end
  end

  defp read(source) do
    cond do
      not File.regular?(source) -> module(source)
      String.ends_with?(source, [".exs", ".ex"]) -> script(source)
      true -> yaml(source)
    end
  end

  defp module(name) do
    mod = Module.concat([name])

    if Code.ensure_loaded?(mod) and function_exported?(mod, :registry, 0) do
      {:ok, mod.registry()}
    else
      {:error, "registry #{name}: not a readable file and not a module exporting registry/0"}
    end
  end

  defp script(path) do
    case Code.eval_file(path) do
      {value, _binding} when is_map(value) -> {:ok, value}
      _other -> {:error, "registry #{path}: the file must evaluate to a map"}
    end
  rescue
    error -> {:error, "registry #{path}: " <> Exception.message(error)}
  end

  defp yaml(path) do
    case YamlElixir.read_from_file!(path) do
      map when is_map(map) -> {:ok, Map.new(map, &entry/1)}
      _other -> {:error, "registry #{path}: the file must hold a name => spec map"}
    end
  rescue
    error -> {:error, "registry #{path}: " <> Exception.message(error)}
  end

  defp entry({name, spec}), do: {to_string(name), spec(to_string(name), spec)}

  defp spec(_name, %{"module" => module}), do: %{module: Module.concat([module])}

  defp spec(_name, %{"cmd" => cmd} = spec) do
    %{
      cmd: cmd,
      args: Enum.map(Map.get(spec, "args", []), &to_string/1),
      env: env(Map.get(spec, "env", %{}))
    }
  end

  defp spec(name, spec),
    do: raise(ArgumentError, "#{name} needs module: or cmd:, got #{inspect(spec)}")

  defp env(pairs) do
    Enum.map(pairs, fn {name, value} ->
      {String.to_charlist(to_string(name)), String.to_charlist(to_string(value))}
    end)
  end
end
