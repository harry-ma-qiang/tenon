import { Plugin } from "./tenon.js";

const plugin = new Plugin({ inject: [] });

const state = { name: "demo", peer: null as string | null, audits: 0 };

plugin.onLoad((config) => {
  state.name = (config.service as string) ?? "demo";
  state.peer = (config.peer as string) ?? null;
  plugin.provide(state.name, {
    ping: () => "pong",
    add: (a: number, b: number) => a + b,
    getenv: (name: string) => process.env[name] ?? "",
    count: () => state.audits,
    big: (size: number) => "x".repeat(size),
    pid: () => process.pid,
  });
  plugin.log(`demo plugin loaded as ${state.name}`);
});

plugin.onUnload(() => {
  plugin.log(`demo plugin ${state.name} unloading`);
});

plugin.on(
  "tools/execute",
  async (args, next) => {
    const request = (args[0] ?? {}) as Record<string, unknown>;
    const command = typeof request.cmd === "string" ? request.cmd : String(request.cmd ?? "");
    if (command.includes("rm -rf")) {
      return { status: "blocked", by: state.name, cmd: command };
    }
    const entry: Record<string, unknown> = { by: state.name };
    if (state.peer) entry.peer = await plugin.svc(state.peer, "ping", []);
    const seen = [...((request.seen as unknown[]) ?? []), entry];
    const result = await next([{ ...request, seen }]);
    return { guarded: state.name, result };
  },
  { mode: "call", prepend: true, arity: 1 },
);

plugin.on(
  "sys/audit",
  () => {
    state.audits += 1;
  },
  { mode: "emit", arity: 1 },
);

plugin.run();
