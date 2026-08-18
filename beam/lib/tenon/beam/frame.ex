defmodule Tenon.Beam.Frame do
  @moduledoc """
  JSON bodies of the base socket frames and the term-to-JSON coercion for kernel data.

  Framing itself is `{:packet, 4}` on the socket: a 4-byte big-endian length followed by
  that many bytes of JSON, the same shape the wire uses on fd 3/4.
  """

  @spec encode(map()) :: iodata()
  def encode(frame), do: Jason.encode_to_iodata!(jsonable(frame))

  @spec decode(binary()) :: {:ok, map()} | :error
  def decode(body) do
    case Jason.decode(body) do
      {:ok, frame} when is_map(frame) -> {:ok, frame}
      _other -> :error
    end
  end

  @spec jsonable(term()) :: term()
  def jsonable(term) when is_pid(term) or is_port(term) or is_reference(term),
    do: inspect(term)

  def jsonable(term) when is_atom(term) and term not in [nil, true, false],
    do: Atom.to_string(term)

  def jsonable(term) when is_map(term),
    do: Map.new(term, fn {key, value} -> {key(key), jsonable(value)} end)

  def jsonable(term) when is_tuple(term), do: term |> Tuple.to_list() |> jsonable()
  def jsonable(term) when is_list(term), do: Enum.map(term, &jsonable/1)
  def jsonable(term), do: term

  defp key(key) when is_binary(key), do: key
  defp key(key) when is_atom(key), do: Atom.to_string(key)
  defp key(key), do: inspect(key)
end
