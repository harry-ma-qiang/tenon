defmodule Tenon.Loader.Tree do
  @moduledoc "Turns composed rows into mount specs and diffs them against the live tree."

  require Logger

  alias Tenon.Loader.Config
  alias Tenon.Loader.Dsh
  alias Tenon.Loader.Group

  @group_name "cordis:group"
  @ignored ["intercept", "isolate"]

  @type node_t :: %{
          id: String.t(),
          name: String.t(),
          parent: String.t() | nil,
          group: boolean(),
          kind: :group | :module | :external | :error,
          disabled: boolean(),
          config: term(),
          spec: map() | nil,
          error: term() | nil
        }

  @type state :: %{
          nodes: %{String.t() => node_t()},
          order: [String.t()],
          fibers: %{String.t() => pid()},
          collapse: %{String.t() => map()},
          warnings: [String.t()]
        }

  @spec empty() :: state()
  def empty, do: %{nodes: %{}, order: [], fibers: %{}, collapse: %{}, warnings: []}

  @spec build([map()], map()) :: %{nodes: [node_t()], collapse: [{String.t(), [map()], fun()}]}
  def build(rows, opts) do
    registry = Map.get(opts, :registry, %{})
    collapse = targets(opts)

    acc = %{
      nodes: [],
      seen: MapSet.new(),
      collapse: Map.new(collapse, fn {k, _m, _f} -> {k, []} end)
    }

    acc = walk(rows, nil, false, {registry, collapse}, acc)

    %{
      nodes: Enum.reverse(acc.nodes),
      collapse:
        Enum.map(collapse, fn {k, _m, f} -> {k, Enum.reverse(Map.fetch!(acc.collapse, k)), f} end)
    }
  end

  @spec targets(map()) :: [{String.t(), (map() -> boolean()), ([map()] -> map())}]
  def targets(opts) do
    user =
      Enum.map(Map.get(opts, :collapse, []), fn {prefix, fun} ->
        {prefix, &String.starts_with?(to_string(Map.get(&1, "name", "")), prefix), fun}
      end)

    case Map.fetch(opts, :dsh) do
      {:ok, dsh} -> [{"dsh", &Dsh.row?/1, &Dsh.spec(dsh, &1)} | user]
      :error -> user
    end
  end

  @spec sync(map(), state(), map()) :: state()
  def sync(ctx, state, built) do
    new_nodes = Map.new(built.nodes, &{&1.id, &1})
    kin = kin(state)
    removed = Enum.reject(state.order, &Map.has_key?(new_nodes, &1))
    fibers = Enum.reduce(Enum.reverse(removed), state.fibers, &drop(&1, &2, kin))

    fibers =
      Enum.reduce(built.nodes, fibers, fn node, acc ->
        step(ctx, node, Map.get(state.nodes, node.id), acc, kin)
      end)

    %{
      nodes: new_nodes,
      order: Enum.map(built.nodes, & &1.id),
      fibers: fibers,
      collapse: collapse(ctx, state.collapse, built.collapse),
      warnings: Map.get(built, :warnings, [])
    }
  end

  @spec dump(state()) :: [map()]
  def dump(state) do
    rows =
      Enum.map(state.order, fn id ->
        node = Map.fetch!(state.nodes, id)
        fiber = Map.get(state.fibers, id)

        %{
          id: id,
          name: node.name,
          kind: node.kind,
          parent: node.parent,
          group: node.group,
          disabled: node.disabled,
          error: node.error,
          fiber: fiber,
          status: status(fiber)
        }
      end)

    rows ++ Enum.map(state.collapse, &collapse_row/1)
  end

  defp collapse_row({key, entry}) do
    %{
      id: Map.fetch!(entry.spec, :id),
      name: key,
      kind: :collapsed,
      parent: nil,
      group: false,
      disabled: false,
      error: nil,
      rows: Enum.map(entry.rows, & &1["id"]),
      fiber: entry.fiber,
      status: status(entry.fiber)
    }
  end

  defp status(nil), do: nil
  defp status(fiber), do: :tenon.status(fiber)

  defp walk(rows, parent, ancestor_off, env, acc) do
    Enum.reduce(rows, acc, fn row, acc -> row(row, parent, ancestor_off, env, acc) end)
  end

  defp row(row, parent, ancestor_off, {registry, collapse} = env, acc) do
    id = to_string(row["id"])
    name = to_string(Map.get(row, "name", ""))

    cond do
      MapSet.member?(acc.seen, id) ->
        fail(acc, id, name, parent, ancestor_off, {:duplicate_id, id})

      group?(row, name) ->
        group(row, id, name, parent, ancestor_off, env, acc)

      key = matching(row, collapse) ->
        harvest(acc, key, row, ancestor_off)

      true ->
        leaf(row, id, name, parent, ancestor_off, registry, acc)
    end
  end

  defp group?(row, name), do: Config.truthy?(row["group"]) or name == @group_name

  defp group(row, id, name, parent, ancestor_off, env, acc) do
    warn_ignored(row, id)

    acc =
      if Config.js_expr?(row["disabled"]) do
        fail(acc, id, name, parent, ancestor_off, :js_disabled_unsupported)
      else
        node = %{
          id: id,
          name: name,
          parent: parent,
          group: true,
          kind: :group,
          disabled: false,
          config: %{id: id},
          spec: %{module: Group, config: %{id: id}, id: id},
          error: nil
        }

        push(acc, node)
      end

    children = if is_list(row["config"]), do: row["config"], else: []
    walk(children, id, ancestor_off or Config.truthy?(row["disabled"]), env, acc)
  end

  defp leaf(row, id, name, parent, ancestor_off, registry, acc) do
    warn_ignored(row, id)
    config = Map.get(row, "config")
    disabled = ancestor_off or Config.truthy?(row["disabled"])

    case resolve(registry, name, config, id, row) do
      {:ok, kind, spec} ->
        node = %{
          id: id,
          name: name,
          parent: parent,
          group: false,
          kind: kind,
          disabled: disabled,
          config: config,
          spec: spec,
          error: nil
        }

        push(acc, node)

      {:error, reason} ->
        fail(acc, id, name, parent, ancestor_off, reason)
    end
  end

  defp warn_ignored(row, id) do
    case Enum.filter(@ignored, &Map.has_key?(row, &1)) do
      [] -> :ok
      keys -> Logger.warning("tenon loader: row #{inspect(id)} ignores #{Enum.join(keys, ", ")}")
    end
  end

  defp resolve(registry, name, config, id, row) do
    case Map.fetch(registry, name) do
      {:ok, %{module: module}} -> native(module, config, id, row)
      {:ok, %{cmd: _cmd} = spec} -> external(spec, config, id, row)
      {:ok, other} -> {:error, {:bad_registry_entry, other}}
      :error -> {:error, {:unknown_name, name}}
    end
  end

  defp native(module, config, id, row) do
    if Config.js_expr_in?(config) or Config.js_expr?(row["disabled"]) do
      {:error, :js_expr_unsupported}
    else
      {:ok, :module, %{module: module, config: config, id: id}}
    end
  end

  defp external(spec, config, id, row) do
    if Config.js_expr?(row["disabled"]) do
      {:error, :js_disabled_unsupported}
    else
      {:ok, :external, Map.merge(spec, %{config: config, id: id})}
    end
  end

  defp harvest(acc, prefix, row, ancestor_off) do
    if ancestor_off do
      acc
    else
      %{acc | collapse: Map.update!(acc.collapse, prefix, &[row | &1])}
    end
  end

  defp matching(row, collapse) do
    Enum.find_value(collapse, fn {key, match?, _fun} -> if match?.(row), do: key end)
  end

  defp fail(acc, id, name, parent, ancestor_off, reason) do
    Logger.error("tenon loader: row #{inspect(id)} (#{name}) failed: #{inspect(reason)}")

    node = %{
      id: id,
      name: name,
      parent: parent,
      group: false,
      kind: :error,
      disabled: ancestor_off,
      config: nil,
      spec: nil,
      error: reason
    }

    push(acc, node)
  end

  defp push(acc, node),
    do: %{acc | nodes: [node | acc.nodes], seen: MapSet.put(acc.seen, node.id)}

  defp kin(state) do
    Enum.group_by(Map.values(state.nodes), & &1.parent, & &1.id)
  end

  defp drop(id, fibers, kin) do
    case Map.fetch(fibers, id) do
      {:ok, pid} ->
        :tenon.unmount(pid)
        Map.drop(fibers, [id | descendants(id, kin)])

      :error ->
        Map.drop(fibers, [id | descendants(id, kin)])
    end
  end

  defp descendants(id, kin) do
    kin |> Map.get(id, []) |> Enum.flat_map(&[&1 | descendants(&1, kin)])
  end

  defp step(ctx, node, nil, fibers, _kin), do: mount(ctx, node, fibers)

  defp step(ctx, node, old, fibers, kin) do
    cond do
      replace?(node, old) -> drop_and_mount(ctx, node, fibers, kin)
      not mountable?(node) -> drop(node.id, fibers, kin)
      not Map.has_key?(fibers, node.id) -> mount(ctx, node, fibers)
      node.config != old.config -> restart(node, fibers)
      true -> fibers
    end
  end

  defp drop_and_mount(ctx, node, fibers, kin) do
    mount(ctx, node, drop(node.id, fibers, kin))
  end

  defp replace?(node, old) do
    node.kind != old.kind or node.name != old.name or node.parent != old.parent or
      node.group != old.group
  end

  defp mountable?(node), do: node.spec != nil and not node.disabled

  defp restart(node, fibers) do
    :ok = :tenon.restart(Map.fetch!(fibers, node.id), node.config)
    fibers
  end

  defp mount(ctx, node, fibers) do
    with true <- mountable?(node),
         {:ok, parent} <- parent_fiber(ctx, node, fibers),
         {:ok, pid} <- start(%{ctx | fiber: parent}, node.spec, node.id) do
      Map.put(fibers, node.id, pid)
    else
      _other -> fibers
    end
  end

  defp parent_fiber(ctx, %{parent: nil}, _fibers), do: {:ok, ctx.fiber}

  defp parent_fiber(_ctx, node, fibers) do
    case Map.fetch(fibers, node.parent) do
      {:ok, pid} ->
        {:ok, pid}

      :error ->
        Logger.error("tenon loader: row #{inspect(node.id)} has no live parent #{node.parent}")
        :error
    end
  end

  defp start(ctx, spec, id) do
    :tenon.mount(ctx, spec)
  rescue
    error ->
      Logger.error("tenon loader: mount of #{inspect(id)} failed: #{inspect(error)}")
      :error
  catch
    :exit, reason ->
      Logger.error("tenon loader: mount of #{inspect(id)} exited: #{inspect(reason)}")
      :error
  end

  defp collapse(ctx, old, built) do
    kept = Enum.map(built, fn {prefix, _rows, _fun} -> prefix end)

    Enum.each(Map.keys(old) -- kept, fn prefix ->
      :tenon.unmount(Map.fetch!(old, prefix).fiber)
    end)

    Enum.reduce(built, %{}, fn {prefix, rows, fun}, acc ->
      collapse_one(ctx, acc, Map.get(old, prefix), {prefix, rows, fun})
    end)
  end

  defp collapse_one(_ctx, acc, nil, {_prefix, [], _fun}), do: acc

  defp collapse_one(_ctx, acc, prev, {_prefix, [], _fun}) do
    :tenon.unmount(prev.fiber)
    acc
  end

  defp collapse_one(ctx, acc, prev, {prefix, rows, fun}) do
    id = "collapse:" <> prefix
    spec = rows |> fun.() |> Map.put_new(:id, id)

    cond do
      prev == nil -> collapse_mount(ctx, acc, prefix, spec, rows, id)
      shape(prev.spec) != shape(spec) -> collapse_remount(ctx, acc, prev, prefix, spec, rows, id)
      prev.spec[:config] != spec[:config] -> collapse_restart(acc, prev, prefix, spec, rows)
      true -> Map.put(acc, prefix, %{prev | rows: rows})
    end
  end

  defp collapse_mount(ctx, acc, prefix, spec, rows, id) do
    case start(ctx, spec, id) do
      {:ok, pid} -> Map.put(acc, prefix, %{spec: spec, rows: rows, fiber: pid})
      :error -> acc
    end
  end

  defp collapse_remount(ctx, acc, prev, prefix, spec, rows, id) do
    :tenon.unmount(prev.fiber)
    collapse_mount(ctx, acc, prefix, spec, rows, id)
  end

  defp collapse_restart(acc, prev, prefix, spec, rows) do
    :ok = :tenon.restart(prev.fiber, spec[:config])
    Map.put(acc, prefix, %{spec: spec, rows: rows, fiber: prev.fiber})
  end

  defp shape(spec), do: Map.delete(spec, :config)
end
