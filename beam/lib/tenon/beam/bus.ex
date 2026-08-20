defmodule Tenon.Beam.Bus do
  @moduledoc """
  Builds bus envelopes (RFC P4 section 2) for the guardian, the `Link` and the
  Logger bridge to publish to base over the Link socket as `bus.publish` frames.
  """

  @levels %{
    emergency: "error",
    alert: "error",
    critical: "error",
    error: "error",
    warning: "warn",
    warn: "warn",
    notice: "info",
    info: "info",
    debug: "debug"
  }

  @spec envelope(String.t(), String.t(), String.t() | nil, map()) :: map()
  def envelope(topic, level, env, payload) do
    base = %{
      "topic" => topic,
      "level" => level,
      "src" => "beam",
      "host" => "",
      "durable" => true,
      "payload" => payload
    }

    if env, do: Map.put(base, "env", env), else: base
  end

  @spec frame(map()) :: map()
  def frame(envelope), do: %{"t" => "bus.publish", "envelope" => envelope}

  @spec level(atom()) :: String.t()
  def level(level), do: Map.get(@levels, level, "info")
end
