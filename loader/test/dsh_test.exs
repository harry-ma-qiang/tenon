defmodule Tenon.Loader.DshTest do
  use ExUnit.Case, async: true

  alias Tenon.Loader.Config
  alias Tenon.Loader.Dsh
  alias Tenon.Loader.Tree

  @bridge Path.expand(Path.join(__DIR__, "fixtures/bridge.js"))
  @fixture Path.join(__DIR__, "fixtures/dsh-collapse.yml")
  @registry %{"tenon:policy" => %{module: Tenon.Loader.Echo}}
  @stale 981_173_106

  defp config(dir, extra \\ %{}) do
    base = %{dsh_home: dir, launcher: ["/bin/true"], bridge: %{module_path: @bridge}}
    Map.merge(base, extra)
  end

  defp harvest(source, config) do
    {rows, []} = Config.compose([source])
    built = Tree.build(rows, %{registry: @registry, dsh: config})
    [{"dsh", harvested, fun}] = built.collapse
    {built.nodes, harvested, fun}
  end

  defp files(dir) do
    profile = Path.join([dir, "profiles", "tenon"])

    {File.read!(Path.join(profile, "package.json")),
     File.read!(Path.join(profile, "cordis.patch.yml"))}
  end

  defp mtimes(dir) do
    profile = Path.join([dir, "profiles", "tenon"])

    Enum.map(["package.json", "cordis.patch.yml"], fn name ->
      File.stat!(Path.join(profile, name), time: :posix).mtime
    end)
  end

  defp age(dir) do
    profile = Path.join([dir, "profiles", "tenon"])
    Enum.each(["package.json", "cordis.patch.yml"], &File.touch!(Path.join(profile, &1), @stale))
  end

  @tag :tmp_dir
  test "harvests dsh rows by name prefix and by the tenon tag", %{tmp_dir: dir} do
    {nodes, rows, _fun} = harvest(@fixture, config(dir))

    assert Enum.map(nodes, & &1.id) == ["policy"]
    assert Enum.map(rows, & &1["id"]) == ["session-title", "fs-local", "hello"]
    assert Enum.at(rows, 0)["name"] == nil
    assert Enum.at(rows, 2)["name"] == "./plugins/hello.mjs"
  end

  @tag :tmp_dir
  test "writes the bundles manifest and the bridge row before the harvested rows", %{tmp_dir: dir} do
    {_nodes, rows, fun} = harvest(@fixture, config(dir))
    spec = fun.(rows)
    {manifest, patch} = files(dir)

    assert manifest == """
           {
             "dependencies": {},
             "dsh": {
               "profile": {
                 "bundles": [
                   "@deepseek-ai/dsh-base"
                 ]
               }
             },
             "name": "dsh-profile-tenon",
             "private": true
           }
           """

    assert patch == """
           - insert:
               - id: tenon-bridge
                 name: '#{@bridge}'
                 config:
                   events:
                     - name: session/created
                       mode: emit
                       pick:
                         - id
                     - name: session/event
                       mode: emit
                       pick:
                         - id
                         - type
                     - name: tools/pre-execute
                       mode: call
                       pick:
                         - name
                         - arguments
                         - callId
                   services:
                     - name: dsh
           - id: session-title
             config:
               fallbackMaxWords: 3
           - insert:
               - id: fs-local
                 name: '@deepseek-ai/dsh-fs-local'
                 config:
                   cwd: !!js process.cwd()
               - id: hello
                 name: './plugins/hello.mjs'
           """

    assert Config.parse!(patch) == Dsh.patches(config(dir), rows)
    assert spec.cmd == "/bin/true"
    assert spec.args == ["--profile", "tenon"]
    assert spec.env == [{~c"DSH_HOME", String.to_charlist(dir)}]
    assert spec.id == "dsh"
  end

  @tag :tmp_dir
  test "the launcher defaults to node and the dsh cli of :dsh_root", %{tmp_dir: dir} do
    config =
      config(dir, %{launcher: nil, dsh_root: "/opt/dsh", node: "/usr/bin/node", profile: "p"})

    {_nodes, rows, fun} = harvest(@fixture, config)
    spec = fun.(rows)

    assert spec.cmd == "/usr/bin/node"
    assert spec.args == ["/opt/dsh/apps/cli/lib/bin.js", "--profile", "p"]
    assert File.exists?(Path.join([dir, "profiles", "p", "cordis.patch.yml"]))
  end

  @tag :tmp_dir
  test "a second identical compose rewrites neither file", %{tmp_dir: dir} do
    {_nodes, rows, fun} = harvest(@fixture, config(dir))
    fun.(rows)
    age(dir)
    stale = mtimes(dir)

    {_nodes, rows, fun} = harvest(@fixture, config(dir))
    assert fun.(rows)
    assert mtimes(dir) == stale
  end

  @tag :tmp_dir
  test "a changed dsh row rewrites only the patch file", %{tmp_dir: dir} do
    {_nodes, rows, fun} = harvest(@fixture, config(dir))
    before = fun.(rows)
    age(dir)
    [manifest_at, patch_at] = mtimes(dir)

    source = String.replace(File.read!(@fixture), "fallbackMaxWords: 3", "fallbackMaxWords: 7")
    {_nodes, rows, fun} = harvest({:entries, Config.parse!(source)}, config(dir))
    spec = fun.(rows)

    assert [^manifest_at, changed] = mtimes(dir)
    assert changed != patch_at
    assert elem(files(dir), 1) =~ "fallbackMaxWords: 7"
    assert spec.config == before.config
  end

  @tag :tmp_dir
  test "a changed bundle list changes the fiber config so the fiber restarts", %{tmp_dir: dir} do
    {_nodes, rows, fun} = harvest(@fixture, config(dir))
    before = fun.(rows)

    config = config(dir, %{bundles: ["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"]})
    {_nodes, rows, fun} = harvest(@fixture, config)

    assert fun.(rows).config != before.config
    assert elem(files(dir), 0) =~ "dsh-headless"
  end

  @tag :tmp_dir
  test "reload: :restart puts the patch digest in the fiber config", %{tmp_dir: dir} do
    config = config(dir, %{reload: :restart})
    {_nodes, rows, fun} = harvest(@fixture, config)
    before = fun.(rows)

    source = String.replace(File.read!(@fixture), "fallbackMaxWords: 3", "fallbackMaxWords: 7")
    {_nodes, rows, fun} = harvest({:entries, Config.parse!(source)}, config)

    assert Map.has_key?(before.config, "patch")
    assert fun.(rows).config != before.config
  end

  @tag :tmp_dir
  test "an existing manifest keeps its own keys and only gains the bundles", %{tmp_dir: dir} do
    profile = Path.join([dir, "profiles", "tenon"])
    File.mkdir_p!(profile)

    File.write!(
      Path.join(profile, "package.json"),
      ~s({"name":"mine","dependencies":{"left-pad":"1.0.0"},"dsh":{"profile":{"other":1}}})
    )

    {_nodes, rows, fun} = harvest(@fixture, config(dir))
    fun.(rows)
    manifest = Jason.decode!(elem(files(dir), 0))

    assert manifest["name"] == "mine"
    assert manifest["dependencies"] == %{"left-pad" => "1.0.0"}
    assert manifest["dsh"]["profile"] == %{"other" => 1, "bundles" => ["@deepseek-ai/dsh-base"]}
  end

  @tag :tmp_dir
  test "a dsh row under a disabled group is not harvested", %{tmp_dir: dir} do
    yaml = """
    - id: g
      name: cordis:group
      group: true
      disabled: true
      config:
        - id: off
          name: '@deepseek-ai/dsh-fs-local'
    """

    {_nodes, rows, _fun} = harvest({:entries, Config.parse!(yaml)}, config(dir))
    assert rows == []
  end
end
