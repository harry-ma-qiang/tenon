defmodule Tenon.Beam.CheckTest do
  use ExUnit.Case, async: false

  alias Tenon.Beam.Check

  setup do
    was = :logger.get_primary_config()
    :logger.set_primary_config(:level, :critical)
    on_exit(fn -> :logger.set_primary_config(was) end)
    :ok
  end

  defp beam, do: to_string(:code.which(:tenon))

  test "the shipped kernel passes every contract point" do
    report = Check.run(beam())

    assert report.ok, "failing points: #{inspect(Enum.reject(report.points, & &1.ok))}"
    assert report.contract == "1"
    assert report.failed == 0

    names = Enum.map(report.points, & &1.name)

    for wanted <- ~w(mount_unmount disposers kill_sweep inject hooks provide_svc
                     socket_fiber frame_cap hot_swap) do
      assert wanted in names
    end
  end

  test "a corrupted beam fails with the reason and runs no point" do
    path = Path.join(System.tmp_dir!(), "tenon-check-corrupt.beam")
    File.write!(path, "not a beam file")
    on_exit(fn -> File.rm(path) end)

    report = Check.run(path)

    refute report.ok
    assert [%{name: "load", ok: false, error: error}] = report.points
    assert error =~ "not a loadable tenon module"
  after
    Check.run(beam())
  end

  test "a contract version this suite does not implement is refused" do
    report = Check.run(beam(), "2")

    refute report.ok
    assert [%{name: "contract", error: error}] = report.points
    assert error =~ "implements 1"
  end
end
