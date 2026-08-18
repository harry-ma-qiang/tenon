defmodule Tenon.Beam.Link.Handlers do
  @moduledoc "The bodies of the `health`, `tree` and `reload` requests base sends a node."

  alias Tenon.Beam.Frame
  alias Tenon.Loader

  @spec opt(map(), atom(), term()) :: term()
  def opt(config, key, default) do
    case Map.fetch(config, key) do
      {:ok, value} -> value
      :error -> Map.get(config, Atom.to_string(key), default)
    end
  end

  @spec result(map()) :: {:ok, term()} | {:error, term()}
  def result(%{"error" => error}), do: {:error, error}
  def result(%{"result" => result}), do: {:ok, result}
  def result(_frame), do: {:ok, nil}

  @spec run(String.t(), map()) :: map()
  def run("health", state) do
    tree = tree(state)

    %{
      "ok" => tree != nil,
      "role" => state.role,
      "env" => state.env,
      "pid" => System.pid(),
      "fibers" => count(tree, fn _node -> true end),
      "failed" => count(tree, &(&1.status == :failed))
    }
  end

  def run("tree", state), do: %{"tree" => Frame.jsonable(tree(state))}

  def run("reload", state) do
    case opt(state.config, :loader, nil) do
      loader when is_pid(loader) -> %{"ok" => Loader.reload(loader) == :ok}
      _other -> %{"ok" => false, "reason" => "no_loader"}
    end
  end

  defp tree(state) do
    case opt(state.config, :kernel, nil) do
      kernel when is_pid(kernel) -> :tenon.tree(kernel)
      _other -> nil
    end
  end

  defp count(nil, _keep?), do: 0

  defp count(node, keep?) do
    below = node |> Map.get(:children, []) |> Enum.reduce(0, &(count(&1, keep?) + &2))
    if keep?.(node), do: below + 1, else: below
  end
end
