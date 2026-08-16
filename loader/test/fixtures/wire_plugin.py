import json
import os
import struct
import sys

wire_in = os.fdopen(3, "rb", 0)
wire_out = os.fdopen(4, "wb", 0)


def readn(n):
    buf = b""
    while len(buf) < n:
        chunk = wire_in.read(n - len(buf))
        if not chunk:
            sys.exit(0)
        buf += chunk
    return buf


def send(frame):
    body = json.dumps(frame).encode()
    wire_out.write(struct.pack(">I", len(body)) + body)


send({"t": "hello", "inject": []})

while True:
    head = readn(4)
    frame = json.loads(readn(struct.unpack(">I", head)[0]))
    if frame["t"] == "load":
        send({"t": "rep", "req": frame["req"], "result": "ok"})
    elif frame["t"] == "unload":
        sys.exit(0)
