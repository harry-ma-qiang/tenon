defmodule Tenon.Beam.RegistryTest do
  use ExUnit.Case, async: true

  alias Tenon.Beam.Registry

  test "a module row becomes a module spec" do
    assert Registry.spec(%{"module" => "Tenon.Loader"}) == %{module: Tenon.Loader}
  end

  test "a spawned plugin keeps its own env and loses TENON_GATEWAY" do
    spec =
      Registry.spec(%{"cmd" => "/usr/bin/python3", "args" => ["p.py"], "env" => [["A", "1"]]})

    assert spec.cmd == "/usr/bin/python3"
    assert spec.args == ["p.py"]
    assert {~c"A", ~c"1"} in spec.env
    assert {~c"TENON_GATEWAY", false} in spec.env
  end
end
