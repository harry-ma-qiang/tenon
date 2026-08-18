defmodule Tenon.Beam.Boot do
  @moduledoc """
  Turns the four boot environment variables into one running node.

  `TENON_ROLE` (`guardian` or `agent`), `TENON_ENV` (the env name), `TENON_BASE_SOCK`
  (the base front door) and `TENON_PROFILE` (the yml entry list). It starts one kernel,
  mounts `Tenon.Loader` on the profile, mounts `Tenon.Beam.Link`, mounts
  `Tenon.Beam.Guardian` when the role is `guardian`, and mounts `Tenon.Beam.Gateway` when
  the role is `agent`.
  """

  use GenServer

  require Logger

  alias Tenon.Beam.Gateway
  alias Tenon.Beam.Guardian
  alias Tenon.Beam.Link
  alias Tenon.Beam.Registry
  alias Tenon.Loader

  @spec node?() :: boolean()
  def node?, do: System.get_env("TENON_ROLE") != nil

  @spec start_link(term()) :: GenServer.on_start()
  def start_link(_args), do: GenServer.start_link(__MODULE__, :boot, name: __MODULE__)

  @spec state() :: map()
  def state, do: GenServer.call(__MODULE__, :state)

  @impl GenServer
  def init(:boot) do
    Process.flag(:trap_exit, true)
    role = System.get_env("TENON_ROLE", "agent")
    env = System.get_env("TENON_ENV", "root")
    {:ok, kernel} = :tenon.start_link()
    ctx = :tenon.root(kernel)
    {:ok, loader} = :tenon.mount(ctx, %{module: Loader, id: "loader", config: profile()})

    {:ok, link} =
      :tenon.mount(ctx, %{module: Link, id: "link", config: link(role, env, kernel, loader)})

    guardian = if role == "guardian", do: mount_guardian(ctx, env), else: nil
    gateway = if role == "agent", do: mount_gateway(ctx, env), else: nil
    Logger.info("tenon node: role #{role}, env #{env}, os pid #{System.pid()}")

    {:ok,
     %{
       kernel: kernel,
       loader: loader,
       link: link,
       guardian: guardian,
       gateway: gateway,
       role: role,
       env: env
     }}
  end

  @impl GenServer
  def handle_call(:state, _from, state), do: {:reply, state, state}

  defp profile do
    layers = layers(System.get_env("TENON_PROFILE"))

    registry =
      Enum.reduce(layers, Registry.builtin(), fn path, acc ->
        Map.merge(acc, Registry.load(Path.join(Path.dirname(path), "registry.yml")))
      end)

    %{layers: layers, registry: registry}
  end

  defp layers(nil), do: []
  defp layers(value), do: String.split(value, ":", trim: true)

  defp link(role, env, kernel, loader) do
    %{
      sock: System.get_env("TENON_BASE_SOCK"),
      role: role,
      env: env,
      kernel: kernel,
      loader: loader
    }
  end

  defp mount_guardian(ctx, env) do
    config = %{
      target: System.get_env("TENON_GUARDIAN_TARGET", "root"),
      interval: number("TENON_GUARDIAN_INTERVAL_MS", 2_000),
      failures: number("TENON_GUARDIAN_FAILURES", 6)
    }

    Logger.info("tenon guardian: watching #{config.target} from #{env}")
    {:ok, fiber} = :tenon.mount(ctx, %{module: Guardian, id: "guardian", config: config})
    fiber
  end

  defp number(name, default) do
    case Integer.parse(System.get_env(name, "")) do
      {value, _rest} when value > 0 -> value
      _other -> default
    end
  end

  defp mount_gateway(ctx, env) do
    address = System.get_env("TENON_GATEWAY", default_gateway(env))
    config = %{address: address}
    {:ok, fiber} = :tenon.mount(ctx, %{module: Gateway, id: "gateway", config: config})
    fiber
  end

  defp default_gateway(env) do
    home = System.get_env("TENON_HOME", Path.join(System.user_home!(), ".tenon"))
    "unix:" <> Path.join([home, "run", "gateway-#{env}.sock"])
  end
end
