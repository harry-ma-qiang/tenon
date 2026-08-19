defmodule Tenon.Beam.Guardian.Probes do
  @moduledoc """
  The fixed core probe set of RFC section 5.2, plus the extra probes base approved.

  Every core probe is one frame to base through the `link` service: does base itself
  answer, is the env alive, is its kernel tree healthy, does its worker answer, does its
  harness answer, is a budget gone, is there a hard-rule violation in its log. The `base`
  probe caches that env's row of the status document, which is what tells the worker and
  harness probes whether base expects an answer at all — an env that is still booting has
  nothing to ping and is not failing. A probe call that takes longer than
  `probe_timeout` is a wedge and fails under its own name. Extra probes are executables
  under `<home>/probes/`; base is what checks their sha256 against its own config and
  passes the approved paths in, so nothing here decides what may run.
  """

  require Logger

  @core [:base, :env, :tree, :worker, :harness, :budgets, :violations]
  @violations ["violation", "budget.exceeded"]
  @tail 200

  @type failure :: {String.t(), term()}

  @spec core() :: [atom()]
  def core, do: @core

  @doc "One pass over every probe. Returns the failures and the advanced state."
  @spec run(map()) :: {[failure()], map()}
  def run(state) do
    {results, state} = Enum.map_reduce(@core, state, &timed/2)
    {failed(results) ++ wedged(results, state) ++ extra(state), state}
  end

  defp timed(name, state) do
    started = System.monotonic_time(:millisecond)
    {outcome, state} = probe(name, state)
    {{name, outcome, System.monotonic_time(:millisecond) - started}, state}
  end

  defp failed(results) do
    Enum.flat_map(results, fn
      {name, {:error, reason}, _ms} -> [{to_string(name), reason}]
      {_name, :ok, _ms} -> []
    end)
  end

  defp wedged(results, state) do
    case Enum.filter(results, fn {_name, _outcome, ms} -> ms >= state.probe_timeout end) do
      [] -> []
      slow -> [{"wedged", Enum.map(slow, fn {name, _o, ms} -> "#{name}:#{ms}ms" end)}]
    end
  end

  defp probe(:base, state) do
    case call(state, "status", %{}) do
      {:ok, status} -> {:ok, %{state | status: row(status, state.target)}}
      other -> {{:error, other}, %{state | status: nil}}
    end
  end

  defp probe(:env, state) do
    {expect(call(state, "health", %{}), &(&1["ok"] == true), "env is not alive"), state}
  end

  defp probe(:tree, state) do
    check = fn result -> get_in(result, ["tree", "status"]) == "active" end
    {expect(call(state, "tree", %{}), check, "the root fiber is not active"), state}
  end

  defp probe(:worker, state), do: {responsive(state, "worker", "worker"), state}
  defp probe(:harness, state), do: {responsive(state, "harness", "loop"), state}

  defp probe(:budgets, state) do
    case get_in(state.status || %{}, ["budget", "halted"]) do
      nil -> {:ok, state}
      reason -> {{:error, reason}, state}
    end
  end

  defp probe(:violations, state) do
    case call(state, "events.tail", %{"after" => state.after, "limit" => @tail}) do
      {:ok, %{"events" => events}} -> violations(events, state)
      other -> {{:error, other}, state}
    end
  end

  # Base's own view of the lifecycle decides whether a ping is owed: an env whose
  # worker is off or still booting is not failing, a failed one is.
  defp responsive(state, key, service) do
    case get_in(state.status || %{}, [key, "state"]) do
      "ready" -> pong(state, service)
      "failed" -> {:error, {"#{key} failed", get_in(state.status, [key, "error"])}}
      _other -> :ok
    end
  end

  defp pong(state, service) do
    params = %{"name" => service, "method" => "ping", "args" => [%{}]}
    expect(call(state, "svc", params), &(&1 == "pong"), "#{service} did not answer ping")
  end

  defp violations(events, state) do
    seen = Enum.reduce(events, state.after, &max(&1["id"], &2))
    state = %{state | after: seen}

    case Enum.filter(events, &(&1["kind"] in @violations)) do
      [] -> {:ok, state}
      rows -> {{:error, Enum.map(rows, & &1["kind"])}, state}
    end
  end

  defp row(status, target) do
    status |> Map.get("nodes", []) |> Enum.find(%{}, &(&1["env"] == target))
  end

  defp expect({:ok, result}, check, reason) do
    if check.(result), do: :ok, else: {:error, {reason, result}}
  end

  defp expect(other, _check, _reason), do: {:error, other}

  defp call(state, method, params) do
    params = Map.put(params, "env", state.target)
    :tenon.svc(state.ctx, :link, :request, [method, params, state.probe_timeout])
  catch
    kind, reason -> {:error, {kind, reason}}
  end

  @doc """
  The extra probes, run as OS commands with the env name as their only argument. A
  non-zero exit is a failure named by the file's basename; a probe that outlives
  `probe_timeout` is killed and counts as one too.
  """
  @spec extra(map()) :: [failure()]
  def extra(state) do
    Enum.flat_map(Map.get(state, :probes, []), &run_extra(&1, state))
  end

  defp run_extra(path, state) do
    task = Task.async(fn -> System.cmd(path, [state.target], stderr_to_stdout: true) end)

    case Task.yield(task, state.probe_timeout) || Task.shutdown(task, :brutal_kill) do
      {:ok, {_out, 0}} -> []
      {:ok, {out, code}} -> [{Path.basename(path), %{"exit" => code, "out" => String.trim(out)}}]
      _other -> [{Path.basename(path), "no answer in #{state.probe_timeout}ms"}]
    end
  rescue
    error -> [{Path.basename(path), Exception.message(error)}]
  end
end
