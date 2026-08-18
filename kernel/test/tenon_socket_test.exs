defmodule TenonSocketTest do
  use ExUnit.Case, async: false

  @plugin Path.join(System.tmp_dir!(), "tenon_socket_plugin.py")

  @source ~S"""
  #!/usr/bin/env python3
  import sys, struct, json, socket

  PATH = sys.argv[1]
  MODE = sys.argv[2] if len(sys.argv) > 2 else "basic"

  SOCK = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
  SOCK.connect(PATH)


  def readn(size):
      buf = b""
      while len(buf) < size:
          chunk = SOCK.recv(size - len(buf))
          if not chunk:
              return None
          buf += chunk
      return buf


  def recv():
      head = readn(4)
      if head is None:
          return None
      body = readn(struct.unpack(">I", head)[0])
      if body is None:
          return None
      return json.loads(body.decode())


  def send(frame):
      body = json.dumps(frame).encode()
      SOCK.sendall(struct.pack(">I", len(body)) + body)


  send({"t": "hello", "inject": []})

  while True:
      frame = recv()
      if frame is None:
          break
      kind = frame.get("t")
      if kind == "load":
          if MODE == "crash":
              sys.exit(3)
          send({"t": "on", "hook": 2, "event": "wire/call", "arity": 1, "mode": "call"})
          send({"t": "provide", "name": "pysvc"})
          send({"t": "rep", "req": frame["req"], "result": "ok"})
      elif kind == "unload":
          sys.exit(0)
      elif kind == "hook":
          args = frame.get("args", [])
          send({"t": "next", "req": frame["req"], "args": [args[0] + 1], "await": True})
      elif kind == "result":
          send({"t": "rep", "req": frame["req"], "result": frame.get("result") + 1})
      elif kind == "svc":
          method = frame.get("method")
          args = frame.get("args", [])
          if method == "add":
              send({"t": "rep", "req": frame["req"], "result": args[0] + args[1]})
          elif method == "ospid":
              import os

              send({"t": "rep", "req": frame["req"], "result": os.getpid()})
          elif method == "big":
              send({"t": "rep", "req": frame["req"], "result": "x" * args[0]})
          elif method == "slow":
              pass
          else:
              send({"t": "rep", "req": frame["req"], "error": "unknown"})
  sys.exit(0)
  """

  setup_all do
    File.write!(@plugin, @source)
    File.chmod!(@plugin, 0o755)
    :ok
  end

  defp kernel(opts \\ %{}) do
    {:ok, k} = :tenon.start_link(opts)
    {k, :tenon.root(k)}
  end

  defp listen do
    path = Path.join(System.tmp_dir!(), "tenon-socket-#{System.unique_integer([:positive])}.sock")
    File.rm(path)
    opts = [:binary, {:packet, 4}, {:ifaddr, {:local, to_charlist(path)}}, active: false]
    {:ok, listen} = :gen_tcp.listen(0, opts)
    {listen, path}
  end

  defp spawn_plugin(path, mode) do
    Port.open({:spawn_executable, String.to_charlist(@plugin)}, [
      :binary,
      :exit_status,
      args: [String.to_charlist(path), String.to_charlist(mode)]
    ])
  end

  defp mounted(ctx, mode) do
    {listen, path} = listen()
    port = spawn_plugin(path, mode)
    {:ok, socket} = :gen_tcp.accept(listen, 5_000)
    :gen_tcp.close(listen)
    {:ok, fiber} = :tenon.mount(ctx, %{socket: socket, id: "gw-test"})
    {fiber, port}
  end

  defp os_pid(port) do
    {:os_pid, pid} = Port.info(port, :os_pid)
    pid
  end

  defp wait_until(fun, timeout \\ 3_000) do
    deadline = System.monotonic_time(:millisecond) + timeout
    poll(fun, deadline)
  end

  defp poll(fun, deadline) do
    cond do
      fun.() ->
        :ok

      System.monotonic_time(:millisecond) > deadline ->
        flunk("wait_until timed out")

      true ->
        Process.sleep(5)
        poll(fun, deadline)
    end
  end

  test "a socket-backed fiber loads, serves a hook and answers a service" do
    {_k, ctx} = kernel()
    {fiber, _port} = mounted(ctx, "basic")

    assert :tenon.status(fiber) == :active
    assert {:tenon_wire, ^fiber, :pysvc} = :tenon.get(ctx, :pysvc)
    assert :tenon.svc(ctx, :pysvc, :add, [1, 2]) == 3
    assert :tenon.call(ctx, :"wire/call", [1], fn v -> v * 10 end) == 21
  end

  test "unmount closes the socket, the plugin exits and the fiber is gone" do
    {_k, ctx} = kernel()
    {fiber, port} = mounted(ctx, "basic")
    assert :tenon.status(fiber) == :active

    os_pid = os_pid(port)
    assert :tenon.unmount(fiber) == :ok
    refute Process.alive?(fiber)
    wait_until(fn -> not File.exists?("/proc/#{os_pid}") end)
  end

  @tag :capture_log
  test "killing the plugin process fails the fiber" do
    {_k, ctx} = kernel()
    {fiber, port} = mounted(ctx, "basic")
    assert :tenon.status(fiber) == :active

    System.cmd("kill", ["-9", Integer.to_string(os_pid(port))])
    wait_until(fn -> :tenon.status(fiber) == :failed end)
    wait_until(fn -> :tenon.get(ctx, :pysvc) == :undefined end)
  end

  test "restarting a socket-backed fiber fails, there is nothing to respawn" do
    {k, ctx} = kernel()
    {fiber, _port} = mounted(ctx, "basic")
    assert :tenon.status(fiber) == :active

    assert :tenon.restart(fiber) == :ok
    wait_until(fn -> :tenon.status(fiber) == :failed end)
    assert %{children: [%{status: :failed, error: :socket_unavailable}]} = :tenon.tree(k)
  end

  @tag :capture_log
  test "an oversize reply over a socket is dropped as frame_too_large" do
    {_k, ctx} = kernel(%{max_frame: 4096})
    {fiber, _port} = mounted(ctx, "basic")
    assert :tenon.status(fiber) == :active

    assert :tenon.svc(ctx, :pysvc, :big, [100_000]) == {:error, :frame_too_large}
    assert Process.alive?(fiber)
    assert :tenon.svc(ctx, :pysvc, :add, [4, 5]) == 9
  end
end
