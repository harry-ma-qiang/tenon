defmodule Tenon.Test.Helpers do
  @moduledoc false

  @spec collected() :: [term()]
  def collected(acc \\ []) do
    receive do
      {:hook, value} -> collected([value | acc])
    after
      0 -> Enum.reverse(acc)
    end
  end

  @spec wait_until((-> boolean()), non_neg_integer()) :: :ok
  def wait_until(fun, timeout \\ 1_000) do
    cond do
      fun.() -> :ok
      timeout <= 0 -> raise "condition not met in time"
      true -> Process.sleep(10) && wait_until(fun, timeout - 10)
    end
  end
end

defmodule Tenon.Test.Echo do
  @moduledoc false
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    send(config.pid, {:loaded, config.tag})
    Tenon.Ctx.effect(ctx, fn -> fn -> send(config.pid, {:disposed, config.tag}) end end)
    :ok
  end
end

defmodule Tenon.Test.Stack do
  @moduledoc false
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Enum.each(config.tags, fn tag -> notify_on_dispose(ctx, config.pid, tag) end)
    :ok
  end

  defp notify_on_dispose(ctx, pid, tag) do
    Tenon.Ctx.effect(ctx, fn -> fn -> send(pid, {:hook, tag}) end end)
  end
end

defmodule Tenon.Test.Registrar do
  @moduledoc false
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.on(ctx, :ping, fn -> :pong end)
    Tenon.Ctx.provide(ctx, config.name, config.impl)
    :ok
  end
end

defmodule Tenon.Test.Db do
  @moduledoc false
  use Tenon.Service, name: :db

  @impl Tenon.Service
  def start(_ctx, config), do: {:ok, config.impl}
end

defmodule Tenon.Test.Consumer do
  @moduledoc false
  use Tenon.Plugin

  @impl Tenon.Plugin
  def inject, do: [:db]

  @impl Tenon.Plugin
  def apply(ctx, config) do
    send(config.pid, {:consumer_loaded, Tenon.Ctx.get(ctx, :db)})
    Tenon.Ctx.effect(ctx, fn -> fn -> send(config.pid, {:consumer_unloaded, config.tag}) end end)
    :ok
  end
end

defmodule Tenon.Test.Boom do
  @moduledoc false
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(_ctx, config) do
    if Agent.get(config.agent, fn fail? -> fail? end), do: raise("boom")
    send(config.pid, {:loaded, :boom})
    :ok
  end
end

defmodule Tenon.Test.Parent do
  @moduledoc false
  use Tenon.Plugin

  @impl Tenon.Plugin
  def apply(ctx, config) do
    Tenon.Ctx.effect(ctx, fn -> fn -> send(config.pid, {:hook, :before_child}) end end)
    {:ok, child} = Tenon.Ctx.plugin(ctx, Tenon.Test.Stack, %{pid: config.pid, tags: [:child]})
    send(config.pid, {:child, child})
    Tenon.Ctx.effect(ctx, fn -> fn -> send(config.pid, {:hook, :after_child}) end end)
    :ok
  end
end
