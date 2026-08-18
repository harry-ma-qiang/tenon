defmodule Tenon.Beam.Link.Handlers do
  @moduledoc "The bodies of the `health`, `tree`, `reload`, `svc` and `plugin` requests."

  alias Tenon.Beam.Frame
  alias Tenon.Beam.Registry
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

  @spec svc(map(), map()) :: {:result, term()} | {:error, term()}
  def svc(frame, state) do
    case opt(state.config, :kernel, nil) do
      kernel when is_pid(kernel) -> call_svc(kernel, frame)
      _other -> {:error, "no_kernel"}
    end
  end

  defp call_svc(kernel, frame) do
    name = String.to_atom(Map.get(frame, "name", ""))
    method = String.to_atom(Map.get(frame, "method", ""))
    args = Map.get(frame, "args", [])
    ctx = :tenon.root(kernel)

    try do
      case :tenon.svc(ctx, name, method, args) do
        {:error, reason} -> {:error, Frame.jsonable(reason)}
        result -> {:result, Frame.jsonable(result)}
      end
    catch
      :error, reason -> {:error, Frame.jsonable(reason)}
    end
  end

  @doc """
  Plugin management for the harness: list, mount, unmount and restart the
  fibers of this node. Mounting takes either a registry-shaped spec
  (`%{"module" => m}` or `%{"cmd" => c, "args" => [..]}`) under the root ctx.
  """
  @spec plugin(map(), map()) :: map()
  def plugin(frame, state) do
    case opt(state.config, :kernel, nil) do
      kernel when is_pid(kernel) -> run_plugin(Map.get(frame, "op", "list"), kernel, frame)
      _other -> %{"ok" => false, "error" => "no_kernel"}
    end
  end

  defp run_plugin("list", kernel, _frame) do
    %{"ok" => true, "plugins" => flatten(:tenon.tree(kernel), [])}
  end

  defp run_plugin("mount", kernel, frame) do
    raw = Map.get(frame, "spec", %{})

    id =
      Map.get(frame, "plugin_id") || Map.get(raw, "id") ||
        "agent-#{System.unique_integer([:positive])}"

    spec =
      raw
      |> Registry.spec()
      |> Map.put(:id, id)
      |> put_config(Map.get(raw, "config") || Map.get(frame, "config"))

    guard(fn ->
      {:ok, fiber} = :tenon.mount(:tenon.root(kernel), spec)
      %{"ok" => true, "id" => id, "fiber" => inspect(fiber), "status" => status(fiber)}
    end)
  end

  defp run_plugin(op, kernel, frame) when op in ["unmount", "restart"] do
    id = to_string(Map.get(frame, "plugin_id", ""))

    case find(:tenon.tree(kernel), id) do
      nil ->
        %{"ok" => false, "error" => "unknown plugin #{id}"}

      fiber ->
        guard(fn -> act(op, fiber, id) end)
    end
  end

  defp run_plugin(op, _kernel, _frame), do: %{"ok" => false, "error" => "unknown op #{op}"}

  defp act("unmount", fiber, id) do
    :tenon.unmount(fiber)
    %{"ok" => true, "id" => id, "op" => "unmount", "status" => status(fiber)}
  end

  defp act("restart", fiber, id) do
    :tenon.restart(fiber)
    %{"ok" => true, "id" => id, "op" => "restart", "status" => status(fiber)}
  end

  defp put_config(spec, nil), do: spec
  defp put_config(spec, config), do: Map.put(spec, :config, config)

  defp status(fiber) do
    if Process.alive?(fiber), do: to_string(:tenon.status(fiber)), else: "disposed"
  end

  defp guard(body) do
    body.()
  rescue
    error -> %{"ok" => false, "error" => Frame.jsonable(Exception.message(error))}
  catch
    _kind, reason -> %{"ok" => false, "error" => Frame.jsonable(reason)}
  end

  defp flatten(nil, acc), do: acc

  defp flatten(node, acc) do
    row = %{
      "id" => Frame.jsonable(Map.get(node, :id)),
      "module" => Frame.jsonable(Map.get(node, :module)),
      "status" => Frame.jsonable(Map.get(node, :status)),
      "fiber" => inspect(Map.get(node, :pid)),
      "error" => Frame.jsonable(Map.get(node, :error))
    }

    Enum.reduce(Map.get(node, :children, []), acc ++ [row], &flatten(&1, &2))
  end

  defp find(nil, _id), do: nil

  defp find(node, id) do
    case to_string(Frame.jsonable(Map.get(node, :id))) == id do
      true -> Map.get(node, :pid)
      false -> Enum.find_value(Map.get(node, :children, []), &find(&1, id))
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
