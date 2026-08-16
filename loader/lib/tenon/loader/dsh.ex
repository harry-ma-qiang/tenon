defmodule Tenon.Loader.Dsh do
  @moduledoc """
  Built-in collapse target: harvested DSH rows become a DSH profile on disk and
  one external fiber running that profile. See `README.md`.
  """

  alias Tenon.Loader.Config

  @prefixes ["@deepseek-ai/dsh-", "dsh-"]
  @tag "dsh"
  @profile "tenon"
  @bundles ["@deepseek-ai/dsh-base"]
  @bridge_id "tenon-bridge"
  @patch_file "cordis.patch.yml"
  @manifest_file "package.json"

  @manifest %{
    "services" => [%{"name" => "dsh"}],
    "events" => [
      %{"name" => "session/created", "mode" => "emit", "pick" => ["id"]},
      %{"name" => "session/event", "mode" => "emit", "pick" => ["id", "type"]},
      %{
        "name" => "tools/pre-execute",
        "mode" => "call",
        "pick" => ["name", "arguments", "callId"]
      }
    ]
  }

  @type config :: %{optional(atom()) => term()}

  @spec row?(map()) :: boolean()
  def row?(row) do
    Map.get(row, "tenon") == @tag or
      String.starts_with?(to_string(Map.get(row, "name", "")), @prefixes)
  end

  @spec spec(config(), [map()]) :: map()
  def spec(config, rows) do
    dir = profile_dir(config)
    File.mkdir_p!(dir)
    manifest = write(Path.join(dir, @manifest_file), manifest_json(config, dir))
    patch = write(Path.join(dir, @patch_file), Config.emit(patches(config, rows)))
    external(config, manifest, patch)
  end

  @spec profile_dir(config()) :: String.t()
  def profile_dir(config), do: Path.join([home(config), "profiles", profile(config)])

  @spec patches(config(), [map()]) :: [map()]
  def patches(config, rows), do: [bridge_patch(config) | row_patches(rows)]

  defp bridge_patch(config) do
    bridge = Map.get(config, :bridge, %{})

    row = %{
      "id" => Map.get(bridge, :id, @bridge_id),
      "name" => Path.expand(Map.fetch!(bridge, :module_path)),
      "config" => Map.get(bridge, :manifest, @manifest)
    }

    %{"insert" => [row]}
  end

  defp row_patches(rows) do
    rows
    |> Enum.map(&Map.drop(&1, ["tenon"]))
    |> Enum.chunk_by(&insert?/1)
    |> Enum.flat_map(fn [first | _] = chunk ->
      if insert?(first), do: [%{"insert" => chunk}], else: chunk
    end)
  end

  defp insert?(row), do: Config.truthy?(row["name"])

  defp manifest_json(config, dir) do
    base = read_json(Path.join(dir, @manifest_file)) || template(config)
    dsh = Map.get(base, "dsh", %{})
    dsh = if is_map(dsh), do: dsh, else: %{}
    profile = Map.get(dsh, "profile", %{})
    profile = if is_map(profile), do: profile, else: %{}
    profile = Map.put(profile, "bundles", Map.get(config, :bundles, @bundles))

    Jason.encode!(Map.put(base, "dsh", Map.put(dsh, "profile", profile)), pretty: true) <> "\n"
  end

  defp template(config) do
    %{"name" => "dsh-profile-" <> profile(config), "private" => true, "dependencies" => %{}}
  end

  defp read_json(path) do
    with {:ok, raw} <- File.read(path),
         {:ok, value} when is_map(value) <- Jason.decode(raw) do
      value
    else
      _other -> nil
    end
  end

  defp write(path, content) do
    if File.read(path) != {:ok, content}, do: File.write!(path, content)
    content
  end

  defp external(config, manifest, patch) do
    [cmd | args] = launcher(config)

    %{
      cmd: cmd,
      args: args ++ ["--profile", profile(config)],
      env: env(config),
      id: Map.get(config, :id, "dsh"),
      config: fiber_config(config, manifest, patch)
    }
  end

  defp fiber_config(config, manifest, patch) do
    base = %{"profile" => profile(config), "manifest" => digest(manifest)}

    if Map.get(config, :reload, :hmr) == :restart,
      do: Map.put(base, "patch", digest(patch)),
      else: base
  end

  defp digest(content), do: Base.encode16(:crypto.hash(:sha256, content), case: :lower)

  defp launcher(config) do
    case Map.get(config, :launcher) do
      [_cmd | _args] = launcher -> launcher
      nil -> [node_bin(config), Path.join(root!(config), "apps/cli/lib/bin.js")]
    end
  end

  defp node_bin(config), do: Map.get(config, :node) || System.find_executable("node") || "node"

  defp root!(config) do
    Map.get(config, :dsh_root) ||
      raise ArgumentError, "tenon loader: dsh collapse needs :dsh_root or :launcher"
  end

  defp env(config) do
    Enum.map([{"DSH_HOME", home(config)} | Map.get(config, :env, [])], fn {name, value} ->
      {String.to_charlist(to_string(name)), String.to_charlist(to_string(value))}
    end)
  end

  defp home(config) do
    case Map.get(config, :dsh_home) do
      nil -> raise ArgumentError, "tenon loader: dsh collapse needs :dsh_home"
      path -> Path.expand(path)
    end
  end

  defp profile(config), do: Map.get(config, :profile, @profile)
end
