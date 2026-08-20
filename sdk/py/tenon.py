import base64
import json
import os
import socket
import struct
import sys
import traceback

WIRE_IN_FD = 3
WIRE_OUT_FD = 4
DEFAULT_MAX_FRAME = 1048576
DEFAULT_DEADLINE_MS = 30000


class TenonError(Exception):
    pass


class FrameTooLarge(TenonError):
    def __init__(self, size, cap):
        super().__init__("frame_too_large")
        self.size = size
        self.cap = cap


class Disconnected(TenonError):
    pass


class _Unloaded(Exception):
    pass


def _env_int(name, fallback):
    try:
        value = int(os.environ.get(name, ""))
    except ValueError:
        return fallback
    return value if value > 0 else fallback


def _reason(exc):
    return str(exc) or type(exc).__name__


def _connect_gateway(address):
    if address.startswith("unix:"):
        path = address[len("unix:"):]
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(path)
        return sock
    if address.startswith("tcp:"):
        host, port = address[len("tcp:"):].rsplit(":", 1)
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.connect((host, int(port)))
        return sock
    raise TenonError("bad TENON_GATEWAY address: %r" % (address,))


# The WebSocket gateway transport (RFC P4.4): one JSON plugin frame per WS text
# message, no length prefix. Lets a browser extension register as a plugin over
# `ws:` without a python side-server. Client frames are masked per RFC 6455.
class _WsClient:
    def __init__(self, address):
        host, port = address[len("ws:"):].rsplit(":", 1)
        self._sock = socket.create_connection((host, int(port)))
        self._buf = b""
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            "GET /ws HTTP/1.1\r\nHost: %s:%s\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\nSec-WebSocket-Key: %s\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n" % (host, port, key)
        )
        self._sock.sendall(request.encode("ascii"))
        self._await_upgrade()

    def _await_upgrade(self):
        while b"\r\n\r\n" not in self._buf:
            data = self._sock.recv(4096)
            if not data:
                raise Disconnected("ws handshake closed")
            self._buf += data
        head, _, rest = self._buf.partition(b"\r\n\r\n")
        if b"101" not in head.split(b"\r\n", 1)[0]:
            raise TenonError("ws upgrade failed: %r" % (head[:80],))
        self._buf = rest

    def send_text(self, payload):
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self._sock.sendall(_ws_header(0x81, len(payload)) + mask + masked)

    def recv_text(self):
        while True:
            frame = self._decode()
            if frame is None:
                data = self._sock.recv(4096)
                if not data:
                    return None
                self._buf += data
                continue
            op, payload = frame
            if op == 0x1:
                return payload
            if op == 0x8:
                return None
            if op == 0x9:
                self._pong(payload)

    def _pong(self, payload):
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self._sock.sendall(_ws_header(0x8A, len(payload)) + mask + masked)

    def _decode(self):
        buf = self._buf
        if len(buf) < 2:
            return None
        op = buf[0] & 0x0F
        masked = (buf[1] & 0x80) != 0
        length = buf[1] & 0x7F
        offset = 2
        if length == 126:
            if len(buf) < 4:
                return None
            length = struct.unpack(">H", buf[2:4])[0]
            offset = 4
        elif length == 127:
            if len(buf) < 10:
                return None
            length = struct.unpack(">Q", buf[2:10])[0]
            offset = 10
        need = offset + (4 if masked else 0) + length
        if len(buf) < need:
            return None
        start = offset + (4 if masked else 0)
        payload = buf[start:start + length]
        if masked:
            key = buf[offset:offset + 4]
            payload = bytes(byte ^ key[i % 4] for i, byte in enumerate(payload))
        self._buf = buf[need:]
        return op, payload

    def close(self):
        try:
            self._sock.close()
        except OSError:
            pass


def _ws_header(first, length):
    if length < 126:
        return bytes([first, 0x80 | length])
    if length < 65536:
        return bytes([first, 0x80 | 126]) + struct.pack(">H", length)
    return bytes([first, 0x80 | 127]) + struct.pack(">Q", length)


def _default_wires():
    address = os.environ.get("TENON_GATEWAY")
    if not address:
        return os.fdopen(WIRE_IN_FD, "rb", 0), os.fdopen(WIRE_OUT_FD, "wb", 0), None
    if address.startswith("ws:"):
        return None, None, _WsClient(address)
    sock = _connect_gateway(address)
    return sock.makefile("rb", buffering=0), sock.makefile("wb", buffering=0), None


class Plugin:
    def __init__(self, inject=(), wire_in=None, wire_out=None):
        self.inject = list(inject)
        self.max_frame = _env_int("TENON_MAX_FRAME", DEFAULT_MAX_FRAME)
        self.deadline_ms = _env_int("TENON_KERNEL_DEADLINE", DEFAULT_DEADLINE_MS)
        self.config = {}
        default_in, default_out, default_ws = (None, None, None)
        if wire_in is None or wire_out is None:
            default_in, default_out, default_ws = _default_wires()
        self._in = wire_in if wire_in is not None else default_in
        self._out = wire_out if wire_out is not None else default_out
        self._ws = default_ws
        self._hooks = {}
        self._services = {}
        self._replies = {}
        self._deferred = []
        self._load_handler = None
        self._unload_handler = None
        self._seq = 0
        self._active = False
        self._unloaded = False

    def log(self, message):
        sys.stderr.write("%s\n" % (message,))
        sys.stderr.flush()

    def on(self, event, mode="emit", prepend=False, arity=1):
        def register(handler):
            hook = self._alloc()
            self._hooks[hook] = handler
            handler.tenon_hook = hook
            self._register({"t": "on", "hook": hook, "event": event,
                            "arity": arity, "mode": mode, "prepend": bool(prepend)})
            return handler

        return register

    def off(self, handler):
        hook = getattr(handler, "tenon_hook", handler)
        self._hooks.pop(hook, None)
        self._register({"t": "off", "hook": hook})

    def provide(self, name, methods):
        self._services[name] = dict(methods)
        self._register({"t": "provide", "name": name})

    def unprovide(self, name):
        self._services.pop(name, None)
        self._register({"t": "unprovide", "name": name})

    def emit(self, event, args=()):
        self._send({"t": "emit", "event": event, "args": list(args)})

    def call(self, event, args=()):
        ident = self._alloc()
        self._send({"t": "call", "id": ident, "event": event, "args": list(args)})
        return self._settle(("rep", ident))

    def svc(self, name, method, args=()):
        ident = self._alloc()
        self._send({"t": "svc", "id": ident, "name": name,
                    "method": method, "args": list(args)})
        return self._settle(("rep", ident))

    def on_load(self, handler):
        self._load_handler = handler
        return handler

    def on_unload(self, handler):
        self._unload_handler = handler
        return handler

    def run(self):
        self._send({"t": "hello", "inject": self.inject})
        try:
            while True:
                frame = self._read()
                if frame is None:
                    break
                self._dispatch(frame)
        except (_Unloaded, Disconnected):
            pass
        except Exception:
            self.log(traceback.format_exc())
        self._shutdown()
        sys.exit(0)

    def _alloc(self):
        self._seq += 1
        return self._seq

    def _register(self, frame):
        if self._active:
            self._send(frame)
        else:
            self._deferred.append(frame)

    def _send(self, frame):
        body = json.dumps(frame).encode("utf-8")
        if len(body) > self.max_frame:
            raise FrameTooLarge(len(body), self.max_frame)
        if self._ws is not None:
            self._ws.send_text(body)
            return
        self._out.write(struct.pack(">I", len(body)) + body)

    def _readn(self, size):
        buf = b""
        while len(buf) < size:
            chunk = self._in.read(size - len(buf))
            if not chunk:
                return None
            buf += chunk
        return buf

    def _read(self):
        if self._ws is not None:
            body = self._ws.recv_text()
            if body is None:
                return None
            return json.loads(body.decode("utf-8"))
        head = self._readn(4)
        if head is None:
            return None
        body = self._readn(struct.unpack(">I", head)[0])
        if body is None:
            return None
        return json.loads(body.decode("utf-8"))

    # Re-entrant by design: waiting for one reply keeps serving inbound frames,
    # so a hook handler may call svc/call and the nested request still completes.
    def _settle(self, slot):
        while slot not in self._replies:
            frame = self._read()
            if frame is None:
                raise Disconnected("wire closed")
            self._dispatch(frame)
        value = self._replies.pop(slot)
        if isinstance(value, _Failure):
            raise TenonError(value.error)
        return value

    def _dispatch(self, frame):
        kind = frame.get("t")
        if kind == "hook":
            self._on_hook(frame)
        elif kind == "svc":
            self._on_svc(frame)
        elif kind == "result":
            self._replies[("result", frame.get("req"))] = frame.get("result")
        elif kind == "rep":
            self._replies[("rep", frame.get("id"))] = _reply_value(frame)
        elif kind == "load":
            self._on_load_frame(frame)
        elif kind == "unload":
            raise _Unloaded()
        else:
            self.log("tenon: ignoring frame %r" % (kind,))

    def _on_load_frame(self, frame):
        req = frame.get("req")
        self.config = frame.get("config") or {}
        self._active = True
        for pending in self._deferred:
            self._send(pending)
        self._deferred = []
        if self._load_handler is None:
            self._send({"t": "rep", "req": req, "result": "ok"})
            return
        self._guard(req, lambda: self._load_handler(self.config) or "ok")

    def _on_hook(self, frame):
        handler = self._hooks.get(frame.get("hook"))
        args = frame.get("args", [])
        req = frame.get("req")
        if frame.get("mode") != "call":
            if handler is None:
                return
            try:
                handler(args)
            except (_Unloaded, Disconnected):
                raise
            except Exception:
                self.log(traceback.format_exc())
            return
        if handler is None:
            self._fail(req, "unknown hook %r" % (frame.get("hook"),))
            return
        self._guard(req, lambda: handler(args, self._nexter(req)))

    def _nexter(self, req):
        def forward(args=()):
            self._send({"t": "next", "req": req, "args": list(args), "await": True})
            return self._settle(("result", req))

        return forward

    def _on_svc(self, frame):
        req = frame.get("req")
        methods = self._services.get(frame.get("name"))
        impl = methods.get(frame.get("method")) if methods else None
        if impl is None:
            self._fail(req, "unknown method %s" % (frame.get("method"),))
            return
        args = frame.get("args", [])
        self._guard(req, lambda: impl(*args))

    def _guard(self, req, body):
        try:
            result = body()
        except (_Unloaded, Disconnected):
            raise
        except Exception as exc:
            self.log(traceback.format_exc())
            self._fail(req, _reason(exc))
            return
        try:
            self._send({"t": "rep", "req": req, "result": result})
        except FrameTooLarge as exc:
            self.log("tenon: reply of %s bytes over cap %s" % (exc.size, exc.cap))
            self._fail(req, "frame_too_large")

    def _fail(self, req, reason):
        if req is None:
            return
        try:
            self._send({"t": "rep", "req": req, "error": reason})
        except FrameTooLarge:
            self._send({"t": "rep", "req": req, "error": "frame_too_large"})

    def _shutdown(self):
        if self._unloaded:
            return
        self._unloaded = True
        if self._unload_handler is None:
            return
        try:
            self._unload_handler()
        except Exception:
            self.log(traceback.format_exc())


class _Failure:
    def __init__(self, error):
        self.error = error


def _reply_value(frame):
    if "error" in frame and frame["error"] is not None:
        return _Failure(frame["error"])
    return frame.get("result")
