defmodule Tenon.Loader.Config do
  @moduledoc "Reads Cordis/DSH config layers and composes them into one entry list."

  @js_key "__jsExpr"
  @js_line ~r/^(?<pre>.*?[:\-][ \t]+)!!js[ \t]+(?<rest>.*)$/
  @blocks ["", "|", ">", "|-", ">-", "|+", ">+"]

  @type row :: %{optional(String.t()) => term()}
  @type layer :: String.t() | {:entries | :patch, String.t() | [row()]}

  @spec compose([layer()]) :: {[row()], [String.t()]}
  def compose(layers) do
    patches = Enum.flat_map(layers, &layer_patches/1)
    {rows, warnings} = apply_entry_patches([], patches)
    {normalize(rows), warnings}
  end

  @spec layer_patches(layer()) :: [row()]
  def layer_patches({:patch, source}), do: rows_of(source)
  def layer_patches({:entries, source}), do: [%{"insert" => rows_of(source)}]

  def layer_patches(path) when is_binary(path) do
    if String.ends_with?(path, [".patch.yml", ".patch.yaml"]) do
      layer_patches({:patch, path})
    else
      layer_patches({:entries, path})
    end
  end

  @spec read(String.t()) :: [row()]
  def read(path), do: path |> File.read!() |> parse!()

  @spec parse!(String.t()) :: [row()]
  def parse!(text) do
    {stripped, exprs} = capture_js(text)

    case YamlElixir.read_from_string!(stripped) do
      rows when is_list(rows) -> restore_js(rows, exprs)
      empty when empty in [nil, "", %{}] -> []
      other -> raise ArgumentError, "config layer must be a list of rows, got #{inspect(other)}"
    end
  end

  @spec js_expr?(term()) :: boolean()
  def js_expr?(%{@js_key => _}), do: true
  def js_expr?(_), do: false

  @spec js_expr_in?(term()) :: boolean()
  def js_expr_in?(value) do
    cond do
      js_expr?(value) -> true
      is_map(value) -> Enum.any?(Map.values(value), &js_expr_in?/1)
      is_list(value) -> Enum.any?(value, &js_expr_in?/1)
      true -> false
    end
  end

  @spec truthy?(term()) :: boolean()
  def truthy?(nil), do: false
  def truthy?(false), do: false
  def truthy?(""), do: false
  def truthy?(0), do: false
  def truthy?(_), do: true

  @spec apply_entry_patches([row()], [row()]) :: {[row()], [String.t()]}
  def apply_entry_patches(data, patches) do
    state = %{data: data, index: index_rows(data, [], %{}), warnings: []}
    state = Enum.reduce(patches, state, &patch(&2, &1))
    {state.data, Enum.reverse(state.warnings)}
  end

  @spec normalize([row()]) :: [row()]
  def normalize(rows), do: assign_ids(rows, "")

  defp rows_of(rows) when is_list(rows), do: rows
  defp rows_of(path) when is_binary(path), do: read(path)

  defp patch(state, patch) do
    {id, rest} = Map.pop(patch, "id")
    {insert, rest} = Map.pop(rest, "insert")
    {name, overrides} = Map.pop(rest, "name")

    cond do
      truthy?(insert) -> insert(state, id, insert)
      not truthy?(id) -> warn(state, "patch: id is required for non-insert patches")
      true -> override(state, id, name, overrides)
    end
  end

  defp insert(state, id, rows) do
    if truthy?(id) do
      insert_into(state, id, rows)
    else
      base = length(state.data)
      %{state | data: state.data ++ rows, index: index_rows(rows, [], state.index, base)}
    end
  end

  defp insert_into(state, id, rows) do
    case Map.fetch(state.index, id) do
      :error ->
        warn(state, "patch insert: entry #{quoted(id)} not found")

      {:ok, path} ->
        target = get_in(state.data, access(path))

        if truthy?(target["group"]) do
          append(state, path, target, rows)
        else
          warn(state, "patch insert: entry #{quoted(id)} is not a group")
        end
    end
  end

  defp append(state, path, target, rows) do
    children = if is_list(target["config"]), do: target["config"], else: []
    data = put_in(state.data, access(path) ++ ["config"], children ++ rows)
    index = index_rows(rows, path ++ ["config"], state.index, length(children))
    %{state | data: data, index: index}
  end

  defp override(state, id, name, overrides) do
    with {:ok, path} <- Map.fetch(state.index, id),
         target = get_in(state.data, access(path)),
         true <- not truthy?(name) or name == target["name"] do
      data = update_in(state.data, access(path), &Map.merge(&1, overrides))
      %{state | data: data}
    else
      :error ->
        warn(state, "patch: entry #{quoted(id)} not found")

      false ->
        warn(state, "patch: name mismatch for #{quoted(id)}, skipping")
    end
  end

  defp warn(state, message), do: %{state | warnings: [message | state.warnings]}

  defp quoted(value), do: inspect(value)

  defp index_rows(rows, base, index, offset \\ 0) do
    rows
    |> Enum.with_index(offset)
    |> Enum.reduce(index, fn {row, i}, acc ->
      path = base ++ [i]
      acc = if truthy?(row["id"]), do: Map.put(acc, row["id"], path), else: acc

      if truthy?(row["group"]) and is_list(row["config"]) do
        index_rows(row["config"], path ++ ["config"], acc)
      else
        acc
      end
    end)
  end

  defp access(path), do: Enum.map(path, &if(is_integer(&1), do: Access.at(&1), else: &1))

  defp assign_ids(rows, prefix) do
    {rows, _counts} =
      Enum.map_reduce(rows, %{}, fn row, counts ->
        name = to_string(Map.get(row, "name", "anon"))
        {id, counts} = row_id(row, name, prefix, counts)
        {nest_ids(Map.put(row, "id", id), id), counts}
      end)

    rows
  end

  defp row_id(row, name, prefix, counts) do
    if truthy?(row["id"]) do
      {to_string(row["id"]), counts}
    else
      n = Map.get(counts, name, 0)
      {"anon:#{prefix}#{name}:#{n}", Map.put(counts, name, n + 1)}
    end
  end

  defp nest_ids(row, id) do
    if truthy?(row["group"]) and is_list(row["config"]) do
      Map.put(row, "config", assign_ids(row["config"], id <> "/"))
    else
      row
    end
  end

  defp capture_js(text) do
    {lines, {_n, exprs}} =
      text
      |> String.split("\n")
      |> Enum.map_reduce({0, %{}}, &capture_line/2)

    {Enum.join(lines, "\n"), exprs}
  end

  defp capture_line(line, {n, exprs}) do
    case Regex.named_captures(@js_line, line) do
      nil ->
        {line, {n, exprs}}

      %{"pre" => pre, "rest" => rest} ->
        key = "__tenon_js_#{n}__"
        {~s(#{pre}"#{key}"), {n + 1, Map.put(exprs, key, js_scalar(rest))}}
    end
  end

  defp js_scalar(rest) do
    trimmed = String.trim(rest)

    cond do
      trimmed in @blocks -> raise ArgumentError, "!!js block scalars are not supported"
      String.starts_with?(trimmed, "\"") -> double_quoted(trimmed)
      String.starts_with?(trimmed, "'") -> single_quoted(trimmed)
      true -> trimmed |> String.replace(~r/\s+#.*$/, "") |> String.trim()
    end
  end

  defp double_quoted(text) do
    case Regex.run(~r/^"((?:[^"\\]|\\.)*)"/, text) do
      [_, body] -> Regex.replace(~r/\\(.)/, body, &unescape/2)
      _ -> raise ArgumentError, "unterminated !!js scalar: #{text}"
    end
  end

  defp unescape(_match, "n"), do: "\n"
  defp unescape(_match, "t"), do: "\t"
  defp unescape(_match, "r"), do: "\r"
  defp unescape(_match, char), do: char

  defp single_quoted(text) do
    case Regex.run(~r/^'((?:[^']|'')*)'/, text) do
      [_, body] -> String.replace(body, "''", "'")
      _ -> raise ArgumentError, "unterminated !!js scalar: #{text}"
    end
  end

  defp restore_js(value, exprs) when is_map(value) do
    Map.new(value, fn {k, v} -> {k, restore_js(v, exprs)} end)
  end

  defp restore_js(value, exprs) when is_list(value), do: Enum.map(value, &restore_js(&1, exprs))

  defp restore_js(value, exprs) when is_binary(value) do
    case Map.fetch(exprs, value) do
      {:ok, code} -> %{@js_key => code}
      :error -> value
    end
  end

  defp restore_js(value, _exprs), do: value
end
