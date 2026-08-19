defmodule Tenon.Loader.Manifest do
  @moduledoc """
  A registry source that reads plugin manifests out of a directory.

  One directory per plugin version, `<dir>/<name>@<version>/manifest.json`, holding
  `{"name", "version", "hash", "cmd", "args", "protocol"}`. Each manifest becomes two
  registry rows — the bare `name` and the exact `name@version` — so a profile row may
  either follow the newest installed version or pin one. A relative `cmd` is resolved
  against the plugin's own directory, and a `protocol` this loader does not speak is
  refused loudly rather than mounted.
  """

  require Logger

  @protocols ["wire", "wire/1", "wire/1.2"]

  @doc "Every manifest under `dir` (or under each of a list of dirs) as `name => spec`."
  @spec load(nil | Path.t() | [Path.t()]) :: map()
  def load(nil), do: %{}
  def load(dirs) when is_list(dirs), do: Enum.reduce(dirs, %{}, &Map.merge(&2, load(&1)))

  def load(dir) do
    case File.ls(dir) do
      {:ok, entries} -> entries |> Enum.sort() |> Enum.flat_map(&rows(dir, &1)) |> Map.new()
      {:error, _reason} -> %{}
    end
  end

  @doc "The manifests themselves, for a caller that wants versions and hashes."
  @spec list(nil | Path.t() | [Path.t()]) :: [map()]
  def list(nil), do: []
  def list(dirs) when is_list(dirs), do: Enum.flat_map(dirs, &list/1)

  def list(dir) do
    case File.ls(dir) do
      {:ok, entries} -> entries |> Enum.sort() |> Enum.flat_map(&read(dir, &1))
      {:error, _reason} -> []
    end
  end

  defp rows(dir, entry) do
    Enum.flat_map(read(dir, entry), fn manifest ->
      spec = spec(Path.join(dir, entry), manifest)
      [{manifest["name"], spec}, {"#{manifest["name"]}@#{manifest["version"]}", spec}]
    end)
  end

  defp read(dir, entry) do
    path = Path.join([dir, entry, "manifest.json"])

    with true <- File.regular?(path),
         {:ok, body} <- File.read(path),
         {:ok, manifest} when is_map(manifest) <- Jason.decode(body),
         :ok <- valid(path, manifest) do
      [manifest]
    else
      false -> []
      {:error, reason} -> warn(path, reason)
      _other -> warn(path, :not_a_manifest)
    end
  end

  defp valid(path, manifest) do
    protocol = Map.get(manifest, "protocol", "wire")

    cond do
      not is_binary(manifest["name"]) or manifest["name"] == "" -> warn(path, :no_name)
      not is_binary(manifest["version"]) or manifest["version"] == "" -> warn(path, :no_version)
      not is_binary(manifest["cmd"]) or manifest["cmd"] == "" -> warn(path, :no_cmd)
      protocol not in @protocols -> warn(path, {:unknown_protocol, protocol})
      true -> :ok
    end
  end

  defp warn(path, reason) do
    Logger.error("tenon loader: manifest #{path} ignored: #{inspect(reason)}")
    []
  end

  defp spec(dir, manifest) do
    %{
      cmd: cmd(dir, manifest["cmd"]),
      args: Enum.map(Map.get(manifest, "args", []), &to_string/1),
      env: env(Map.get(manifest, "env", []))
    }
  end

  defp cmd(dir, cmd) do
    case Path.type(cmd) do
      :absolute -> cmd
      _relative -> Path.expand(cmd, dir)
    end
  end

  defp env(pairs) do
    Enum.map(pairs, fn [name, value] ->
      {String.to_charlist(to_string(name)), String.to_charlist(to_string(value))}
    end)
  end
end
