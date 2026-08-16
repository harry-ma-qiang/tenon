#!/usr/bin/env python3
import os

from tenon import Plugin

plugin = Plugin(inject=[])

state = {"name": "demo", "peer": None, "audits": 0}


@plugin.on_load
def load(config):
    state["name"] = config.get("service", "demo")
    state["peer"] = config.get("peer")
    plugin.provide(state["name"], {
        "ping": lambda: "pong",
        "add": lambda a, b: a + b,
        "getenv": lambda name: os.environ.get(name, ""),
        "count": lambda: state["audits"],
        "big": lambda size: "x" * size,
        "pid": lambda: os.getpid(),
    })
    plugin.log("demo plugin loaded as %s" % state["name"])


@plugin.on_unload
def unload():
    plugin.log("demo plugin %s unloading" % state["name"])


@plugin.on("tools/execute", mode="call", prepend=True, arity=1)
def guard(args, next):
    request = args[0] if args else {}
    command = request.get("cmd", "") if isinstance(request, dict) else str(request)
    if "rm -rf" in command:
        return {"status": "blocked", "by": state["name"], "cmd": command}
    entry = {"by": state["name"]}
    if state["peer"]:
        entry["peer"] = plugin.svc(state["peer"], "ping", [])
    seen = list(request.get("seen", [])) + [entry]
    result = next([dict(request, seen=seen)])
    return {"guarded": state["name"], "result": result}


@plugin.on("sys/audit", mode="emit", arity=1)
def audit(args):
    state["audits"] += 1


plugin.run()
