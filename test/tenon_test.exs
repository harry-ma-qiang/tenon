defmodule WLTest do
  use ExUnit.Case, async: true

  test "application starts and supervisor is alive" do
    assert {:ok, _apps} = Application.ensure_all_started(:tenon)
    assert is_pid(Process.whereis(Tenon.Supervisor))
  end
end
