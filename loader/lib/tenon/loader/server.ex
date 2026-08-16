defmodule Tenon.Loader.Server do
  @moduledoc "Ops process for one loader fiber: reload and dump, keyed by the fiber pid."

  use GenServer

  alias Tenon.Loader.Tree

  @registry Tenon.Loader.Registry

  @spec start(map(), map(), Tree.state()) :: {:ok, pid()} | {:error, term()}
  def start(ctx, config, state) do
    GenServer.start(__MODULE__, {ctx, config, state}, name: via(ctx.fiber))
  end

  @spec call(pid(), term()) :: term()
  def call(loader, message), do: GenServer.call(via(loader), message, :infinity)

  @spec stop(pid()) :: :ok
  def stop(pid) do
    if Process.alive?(pid), do: GenServer.stop(pid), else: :ok
  end

  @impl GenServer
  def init({ctx, config, state}) do
    Process.monitor(ctx.fiber)
    {:ok, %{ctx: ctx, config: config, state: state}}
  end

  @impl GenServer
  def handle_call(:reload, _from, server) do
    state = Tenon.Loader.compose(server.ctx, server.config, server.state)
    {:reply, :ok, %{server | state: state}}
  end

  def handle_call(:dump, _from, server) do
    {:reply, Tree.dump(server.state), server}
  end

  def handle_call(:state, _from, server) do
    {:reply, server.state, server}
  end

  @impl GenServer
  def handle_info({:DOWN, _ref, :process, _pid, _reason}, server) do
    {:stop, :normal, server}
  end

  def handle_info(_message, server), do: {:noreply, server}

  defp via(fiber), do: {:via, Registry, {@registry, fiber}}
end
