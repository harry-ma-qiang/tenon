defmodule Tenon.Beam.LogBridge do
  @moduledoc """
  A `:logger` handler that turns this node's Elixir Logger events into `log/<node>`
  bus envelopes and hands them to a publish function (the `Link`, in a live node).
  Mounted only in a node that has a Link; removed with the Link.

  Loop-safe: it never logs, it drops events raised by the bridge modules
  themselves and by anything tagged with the `:nolog` domain, and every failure is
  swallowed so a bad event can never bring the logger down.
  """

  alias Tenon.Beam.Bus

  @bridge_modules [__MODULE__, Tenon.Beam.Link.Server, Tenon.Beam.Bus]

  @spec attach((map() -> any()), String.t()) :: :ok
  def attach(publish, node) do
    config = %{level: :info, config: %{publish: publish, node: node}}
    _ = :logger.add_handler(handler_id(node), __MODULE__, config)
    :ok
  end

  @spec detach(String.t()) :: :ok
  def detach(node) do
    _ = :logger.remove_handler(handler_id(node))
    :ok
  end

  @spec log(map(), map()) :: :ok
  def log(event, %{config: %{publish: publish, node: node}}) do
    if forward?(event) do
      envelope =
        Bus.envelope("log/#{node}", Bus.level(event.level), node, %{
          "msg" => message(event),
          "level" => Atom.to_string(event.level)
        })

      publish.(envelope)
    end

    :ok
  rescue
    _error -> :ok
  catch
    _kind, _reason -> :ok
  end

  def log(_event, _config), do: :ok

  defp forward?(event) do
    meta = Map.get(event, :meta, %{})
    not bridge_source?(meta) and :nolog not in List.wrap(Map.get(meta, :domain, []))
  end

  defp bridge_source?(meta) do
    case Map.get(meta, :mfa) do
      {module, _fun, _arity} -> module in @bridge_modules
      _other -> false
    end
  end

  defp message(%{msg: {:string, chardata}}), do: IO.chardata_to_string(chardata)
  defp message(%{msg: {:report, report}}), do: inspect(report)

  defp message(%{msg: {format, args}}) do
    format |> :io_lib.format(args) |> IO.chardata_to_string()
  rescue
    _error -> inspect({format, args})
  end

  defp message(_event), do: ""

  defp handler_id(node), do: String.to_atom("tenon_log_bridge_" <> node)
end
