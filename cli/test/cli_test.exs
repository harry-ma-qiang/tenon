defmodule Tenon.CLITest do
  use ExUnit.Case, async: false

  import ExUnit.CaptureIO

  alias Tenon.CLI
  alias Tenon.CLI.Registry

  @loader Path.expand("../../loader/test/fixtures", __DIR__)
  @registry Path.expand("fixtures/registry.yml", __DIR__)
  @dsh_home "/tmp/tenon-cli-test-home"

  defp layer(name), do: Path.join(@loader, name)

  defp run(argv), do: with_io(fn -> CLI.exec(argv) end)

  test "dump resolves every row of a tree and mounts nothing" do
    {code, output} = run(["dump", layer("tree.yml"), "--registry", @registry])

    assert code == 0
    assert output =~ ~r/alpha\s+external\s+probe/
    assert output =~ ~r/grp\s+group\s+cordis:group/
    assert output =~ ~r/beta\s+external\s+probe\s+grp/
    refute output =~ "active"
  end

  test "dump applies a patch layer and collapses dsh rows" do
    File.rm_rf!(@dsh_home)

    {code, output} =
      run([
        "dump",
        layer("dsh-cordis.yml"),
        layer("dsh-cordis.patch.yml"),
        "--registry",
        @registry,
        "--dsh-home",
        @dsh_home
      ])

    assert code == 0
    assert output =~ ~r/policy\s+external\s+tenon:policy/
    assert output =~ "collapsed dsh: settings, credentials"
    refute File.exists?(@dsh_home)
  end

  test "check is green when every name resolves" do
    {code, output} = run(["check", layer("tree.yml"), "--registry", @registry])

    assert code == 0
    assert output =~ "4 rows, 0 errors, 0 warnings"
  end

  test "check is red on an unknown name" do
    {code, output} = run(["check", layer("tree.yml")])

    assert code == 1
    assert output =~ ~s/error: row alpha (probe) {:unknown_name, "probe"}/
    assert output =~ "4 rows, 3 errors, 0 warnings"
  end

  test "a missing layer and an unknown command fail with usage" do
    assert capture_io(:stderr, fn -> assert CLI.exec(["check", "nope.yml"]) == 1 end) =~
             "no such layer: nope.yml"

    assert capture_io(:stderr, fn -> assert CLI.exec(["wat"]) == 1 end) =~ "usage:"
  end

  test "the builtin registry carries the group name" do
    assert Registry.builtin()["cordis:group"] == %{module: Tenon.Loader.Group}
    assert {:error, message} = Registry.load("No.Such.Module")
    assert message =~ "registry/0"
  end
end
