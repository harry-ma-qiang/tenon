defmodule Tenon.CLI.Signals do
  @moduledoc """
  Routes SIGHUP (reload) and SIGTERM/SIGQUIT (stop) to the process running `tenon start`.

  It replaces the default OTP signal handler, so `init:stop/0` never races the graceful
  unmount, and the crash-dump behaviour of SIGUSR1 is gone. SIGINT is not routable
  (`os:set_signal/2` refuses it): Ctrl-C stays with the emulator break handler.
  """

  @behaviour :gen_event

  @signals [:sighup, :sigterm, :sigquit]

  @spec install(pid()) :: :ok
  def install(owner) do
    :gen_event.delete_handler(:erl_signal_server, :erl_signal_handler, [])
    :ok = :gen_event.add_handler(:erl_signal_server, __MODULE__, owner)
    Enum.each(@signals, &:os.set_signal(&1, :handle))
  end

  @impl :gen_event
  def init(owner), do: {:ok, owner}

  @impl :gen_event
  def handle_event(:sighup, owner) do
    send(owner, :sighup)
    {:ok, owner}
  end

  def handle_event(signal, owner) when signal in [:sigterm, :sigquit] do
    send(owner, :stop)
    {:ok, owner}
  end

  def handle_event(_signal, owner), do: {:ok, owner}

  @impl :gen_event
  def handle_call(_request, owner), do: {:ok, :ok, owner}

  @impl :gen_event
  def handle_info(_message, owner), do: {:ok, owner}
end
