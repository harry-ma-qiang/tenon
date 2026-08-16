ExUnit.start()

defmodule Tenon.Loader.Probe do
  @moduledoc false

  @spec reset() :: :ok
  def reset do
    if pid = Process.whereis(__MODULE__), do: Agent.stop(pid)
    {:ok, _pid} = Agent.start(fn -> [] end, name: __MODULE__)
    :ok
  end

  @spec push(term()) :: :ok
  def push(event), do: Agent.update(__MODULE__, &[event | &1])

  @spec events() :: [term()]
  def events, do: Agent.get(__MODULE__, &Enum.reverse/1)
end

defmodule Tenon.Loader.Echo do
  @moduledoc false

  alias Tenon.Loader.Probe

  @spec inject() :: []
  def inject, do: []

  @spec load(map(), term()) :: {:ok, (-> :ok)}
  def load(_ctx, config) do
    Probe.push({:load, config})
    {:ok, fn -> Probe.push({:unload, config}) end}
  end
end
