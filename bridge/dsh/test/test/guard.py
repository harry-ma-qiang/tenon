#!/usr/bin/env python3
import json

from tenon import Plugin

plugin = Plugin(inject=[])

state = {"allowed": 0, "denied": 0, "sessions": [], "events": 0}

BANNED = "rm -rf"


@plugin.on_load
def load(config):
    plugin.provide("guard", {
        "allowed": lambda: state["allowed"],
        "denied": lambda: state["denied"],
        "sessions": lambda: list(state["sessions"]),
        "events": lambda: state["events"],
    })
    plugin.log("tenon guard loaded, banning %r" % BANNED)


@plugin.on("tools/pre-execute", mode="call", prepend=True, arity=1)
def pre_execute(args, next):
    call = args[0] if args else {}
    payload = json.dumps(call.get("arguments", {}))
    if BANNED in payload:
        state["denied"] += 1
        return {"deny": "tenon guard: %r is not allowed in %s" % (BANNED, call.get("name"))}
    state["allowed"] += 1
    return next([call])


@plugin.on("session/created", mode="emit", arity=1)
def session_created(args):
    session = args[0] if args else {}
    state["sessions"].append(session.get("id"))


@plugin.on("session/event", mode="emit", arity=2)
def session_event(args):
    state["events"] += 1


plugin.run()
