defmodule Tenon.Beam.Check.Plugins do
  @moduledoc """
  The plugin modules the contract suite mounts.

  They live in the release beside the suite, because the contract has to be
  checkable on an installed machine where no development tree exists.
  """

  defmodule Echo do
    @moduledoc false
    def load(ctx, %{pid: pid, tag: tag}) do
      send(pid, {:loaded, tag})
      :tenon.effect(ctx, fn -> fn -> send(pid, {:disposed, tag}) end end)
      :ok
    end
  end

  defmodule Stack do
    @moduledoc false
    def load(ctx, %{pid: pid, tags: tags}) do
      Enum.each(tags, &disposer(ctx, pid, &1))
      :ok
    end

    defp disposer(ctx, pid, tag) do
      :tenon.effect(ctx, fn -> fn -> send(pid, {:disposed, tag}) end end)
    end
  end

  defmodule Db do
    @moduledoc false
    def load(ctx, %{impl: impl}) do
      :tenon.provide(ctx, :db, impl)
      :ok
    end
  end

  defmodule Consumer do
    @moduledoc false
    def inject, do: [:db]

    def load(ctx, %{pid: pid}) do
      send(pid, {:consumer_loaded, :tenon.get(ctx, :db)})
      :tenon.effect(ctx, fn -> fn -> send(pid, {:consumer_unloaded, :db}) end end)
      :ok
    end
  end

  defmodule Registrar do
    @moduledoc false
    def load(ctx, %{name: name, impl: impl}) do
      :tenon.on(ctx, :ping, fn -> :pong end)
      :tenon.provide(ctx, name, impl)
      :ok
    end
  end

  defmodule Adder do
    @moduledoc false
    def add(a, b), do: a + b
  end
end
