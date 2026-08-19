defmodule Tenon.Beam.Check do
  @moduledoc """
  `tenon check kernel`: the kernel contract suite, shipped as a runtime artifact.

  Base runs `bin/tenon_beam eval 'Tenon.Beam.Check.main()'` with
  `TENON_CHECK_BEAM` naming the `tenon.beam` to check. The named module is
  purged and loaded into this fresh node, every contract point runs against it,
  and one JSON document goes to stdout. The exit status is 0 when every point
  passed.

  The contract is versioned: this suite implements `TENON_KERNEL_CONTRACT=1`
  and refuses a caller asking for a version it does not implement. Changing the
  wire or the API means a new version, and a new version needs a human — L1 in
  RFC section 10.
  """

  alias Tenon.Beam.Check.Points
  alias Tenon.Beam.Check.Wire

  @contract "1"

  @spec contract() :: String.t()
  def contract, do: @contract

  @spec main() :: no_return()
  def main do
    :logger.set_primary_config(:level, :critical)
    report = run(System.get_env("TENON_CHECK_BEAM"), System.get_env("TENON_KERNEL_CONTRACT"))
    IO.puts(Jason.encode!(report))
    System.halt(if report.ok, do: 0, else: 1)
  end

  @doc """
  Loads `beam` and runs every contract point against it. A missing path checks
  the module this node already runs, which is what verifies the shipped kernel.
  """
  @spec run(nil | String.t(), nil | String.t()) :: map()
  def run(beam, wanted \\ nil) do
    case asked(wanted) do
      :ok -> check(beam)
      {:error, reason} -> report(beam, [%{name: "contract", ok: false, error: reason}])
    end
  end

  defp asked(nil), do: :ok
  defp asked(""), do: :ok
  defp asked(@contract), do: :ok

  defp asked(other) do
    {:error,
     "kernel contract #{other} is not implemented by this suite (implements #{@contract})"}
  end

  defp check(beam) do
    where = beam || to_string(:code.which(:tenon))

    case load(beam) do
      {:ok, binary} -> report(where, Enum.map(points(), &point(&1, {to_charlist(where), binary})))
      {:error, reason} -> report(where, [%{name: "load", ok: false, error: reason}])
    end
  end

  defp points do
    Enum.map(Points.names(), &{&1, :vm}) ++ Enum.map(Wire.names(), &{&1, :wire})
  end

  defp point({name, kind}, binary) do
    case outcome(kind, name, binary) do
      :ok -> %{name: to_string(name), ok: true}
      {:error, reason} -> %{name: to_string(name), ok: false, error: text(reason)}
    end
  rescue
    error -> %{name: to_string(name), ok: false, error: text(Exception.message(error))}
  catch
    kind, reason -> %{name: to_string(name), ok: false, error: text({kind, reason})}
  end

  defp outcome(:vm, name, _binary), do: Points.run(name)
  defp outcome(:wire, name, binary), do: Wire.run(name, binary)

  defp text(reason) when is_binary(reason), do: reason
  defp text(reason), do: inspect(reason)

  # The module has to come off disk as bytes and be loaded by hand: a release
  # runs in embedded mode and a candidate beam is not on any code path.
  defp load(nil), do: {:ok, current()}

  defp load(path) do
    case File.read(path) do
      {:ok, binary} -> loaded(path, binary)
      {:error, reason} -> {:error, "read #{path}: #{:file.format_error(reason)}"}
    end
  end

  defp loaded(path, binary) do
    :code.purge(:tenon)

    case :code.load_binary(:tenon, to_charlist(path), binary) do
      {:module, :tenon} -> {:ok, binary}
      other -> {:error, "#{path} is not a loadable tenon module: #{inspect(other)}"}
    end
  end

  defp current do
    case :code.which(:tenon) do
      path when is_list(path) -> File.read!(to_string(path))
      _other -> ""
    end
  end

  defp report(beam, rows) do
    failed = Enum.count(rows, &(not &1.ok))

    %{
      contract: @contract,
      beam: beam || to_string(:code.which(:tenon)),
      ok: failed == 0,
      passed: length(rows) - failed,
      failed: failed,
      points: rows
    }
  end
end
