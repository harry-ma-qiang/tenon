defmodule Tenon.CLI do
  @moduledoc """
  The `tenon` escript: start, dump or check a Cordis/DSH config tree on the atom kernel.

  `start` mounts `Tenon.Loader` on a fresh kernel and stays alive; `dump` and `check`
  compose the layers and resolve every row without mounting anything.
  """

  alias Tenon.CLI.Registry
  alias Tenon.CLI.Signals
  alias Tenon.Loader
  alias Tenon.Loader.Config
  alias Tenon.Loader.Tree

  @switches [
    registry: :string,
    dsh_home: :string,
    dsh_root: :string,
    dsh_bridge: :string,
    profile: :string
  ]

  @header ["id", "kind", "name", "parent", "detail"]

  @usage """
  usage:
    tenon start <layer.yml>...   mount the composed tree and stay alive
    tenon dump  <layer.yml>...   composed rows with resolved kinds, nothing mounted
    tenon check <layer.yml>...   compose only, exit 1 on any bad row

  options:
    --registry MOD_OR_FILE   extra "name => spec" rows (.yml, .exs, or a module exporting registry/0)
    --dsh-home DIR           enable the DSH collapse target and write its profile there
    --dsh-root DIR           the DSH checkout, for the default node launcher
    --dsh-bridge FILE        tenon-bridge plugin.js (default $TENON_DSH_BRIDGE)
    --profile NAME           DSH profile name (default "tenon")

  A layer ending in .patch.yml is a patch list, any other layer is an entry list.
  """

  @spec main([String.t()]) :: :ok | no_return()
  def main(argv), do: argv |> exec() |> halt()

  @spec exec([String.t()]) :: 0 | 1
  def exec(["start" | argv]), do: run(argv, &start/1)
  def exec(["dump" | argv]), do: run(argv, &dump/1)
  def exec(["check" | argv]), do: run(argv, &check/1)
  def exec(_argv), do: fail(@usage)

  defp run(argv, command) do
    {opts, layers, invalid} = OptionParser.parse(argv, strict: @switches)

    with :ok <- validate(layers, invalid),
         {:ok, registry} <- Registry.load(opts[:registry]) do
      command.(config(layers, registry, opts))
    else
      {:error, message} -> fail(message)
    end
  end

  defp validate(_layers, [{flag, _value} | _rest]), do: {:error, "unknown option " <> flag}
  defp validate([], []), do: {:error, @usage}

  defp validate(layers, []) do
    case Enum.reject(layers, &File.regular?/1) do
      [] -> :ok
      missing -> {:error, "no such layer: " <> Enum.join(missing, ", ")}
    end
  end

  defp config(layers, registry, opts) do
    base = %{layers: layers, registry: registry}
    if opts[:dsh_home], do: Map.put(base, :dsh, dsh(opts)), else: base
  end

  defp dsh(opts) do
    bridge = opts[:dsh_bridge] || System.get_env("TENON_DSH_BRIDGE") || "plugin.js"
    home = %{dsh_home: opts[:dsh_home], dsh_root: opts[:dsh_root]}
    Map.merge(home, %{profile: opts[:profile] || "tenon", bridge: %{module_path: bridge}})
  end

  defp start(config) do
    {:ok, kernel} = :tenon.start_link()
    spec = %{module: Loader, config: config, id: "loader"}
    {:ok, loader} = :tenon.mount(:tenon.root(kernel), spec)
    IO.puts(table(Loader.dump(loader)))
    Signals.install(self())
    IO.puts("\ntenon: os pid #{System.pid()}, SIGHUP reloads, SIGTERM stops, Ctrl-C aborts")
    serve(loader, kernel)
  end

  defp serve(loader, kernel) do
    receive do
      :sighup ->
        :ok = Loader.reload(loader)
        IO.puts("\ntenon: reloaded\n" <> table(Loader.dump(loader)))
        serve(loader, kernel)

      :stop ->
        :tenon.unmount(loader)
        :tenon.stop(kernel)
        0
    end
  end

  defp dump(config) do
    {built, warnings} = compose(config)
    IO.puts(table(built.nodes))
    Enum.each(collapsed(built), &IO.puts/1)
    Enum.each(warnings, &IO.puts("warning: " <> &1))
    0
  end

  defp check(config) do
    {built, warnings} = compose(config)
    errors = Enum.filter(built.nodes, &(&1.kind == :error))
    Enum.each(errors, &IO.puts("error: row #{&1.id} (#{&1.name}) #{inspect(&1.error)}"))
    Enum.each(warnings, &IO.puts("warning: " <> &1))
    IO.puts("#{length(built.nodes)} rows, #{length(errors)} errors, #{length(warnings)} warnings")
    if errors == [], do: 0, else: 1
  end

  defp compose(config) do
    Logger.configure(level: :none)
    {rows, warnings} = Config.compose(config.layers)
    {Tree.build(rows, config), warnings}
  end

  defp collapsed(built) do
    for {prefix, rows, _fun} <- built.collapse, rows != [] do
      "collapsed #{prefix}: " <> Enum.map_join(rows, ", ", &to_string(&1["id"]))
    end
  end

  defp table(rows) do
    body = Enum.map(rows, &cells/1)
    widths = widths([@header | body])
    Enum.map_join([@header | body], "\n", &line(&1, widths))
  end

  defp cells(row) do
    kind = to_string(row.kind) <> if(Map.get(row, :disabled), do: " off", else: "")
    [to_string(row.id), kind, to_string(row.name), to_string(row.parent || "-"), detail(row)]
  end

  defp detail(%{error: error}) when not is_nil(error), do: inspect(error)
  defp detail(row), do: to_string(Map.get(row, :status) || "-")

  defp widths(rows) do
    Enum.reduce(rows, List.duplicate(0, length(@header)), fn cells, acc ->
      Enum.zip_with(cells, acc, fn cell, width -> max(String.length(cell), width) end)
    end)
  end

  defp line(cells, widths) do
    cells
    |> Enum.zip_with(widths, &String.pad_trailing/2)
    |> Enum.join("  ")
    |> String.trim_trailing()
  end

  defp fail(message) do
    IO.puts(:stderr, message)
    1
  end

  defp halt(0), do: :ok
  defp halt(code), do: System.halt(code)
end
