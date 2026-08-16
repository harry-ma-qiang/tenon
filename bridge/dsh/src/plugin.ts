import { fstatSync } from "node:fs";
import { Plugin } from "#sdk";
import {
  installMirrors,
  reason,
  type Disposer,
  type EventSpec,
  type HostContext,
  type Logger,
  type MirrorInfo,
  type Mirrors,
} from "./mirror.js";

const WIRE_IN_FD = 3;
const WIRE_OUT_FD = 4;
const MAX_TEXT = 4000;
const DEFAULT_SERVICE = "dsh";
const DEFAULT_DEMO_TOOL = "tenon_echo";

export const name = "tenon-bridge";

type Handler = (...args: any[]) => unknown;

export interface ServiceSpec {
  name?: string;
  methods?: string[] | Record<string, string>;
}

export interface BridgeConfig {
  events?: EventSpec[];
  services?: ServiceSpec[];
  demoTool?: string | boolean;
}

interface ToolsService {
  schemas(): { name: string; description?: string }[];
  register(definition: unknown): Disposer;
  execute(input: Record<string, unknown>): Promise<Record<string, unknown>>;
}

interface SessionsService {
  list(): { id: unknown }[];
  create(): { id: unknown };
}

interface AgentsService {
  list(): Record<string, unknown>[];
}

function log(line: string): void {
  process.stderr.write(`${line}\n`);
}

function wireReady(): boolean {
  for (const fd of [WIRE_IN_FD, WIRE_OUT_FD]) {
    try {
      fstatSync(fd);
    } catch {
      return false;
    }
  }
  return true;
}

function text(value: unknown): string {
  const rendered = typeof value === "string" ? value : String(value);
  return rendered.length > MAX_TEXT ? `${rendered.slice(0, MAX_TEXT)}...` : rendered;
}

function service<T>(host: HostContext, wanted: string): T {
  const impl = host.get(wanted);
  if (impl === undefined || impl === null) throw new Error(`dsh service "${wanted}" is not available`);
  return impl as T;
}

function blocksToText(result: Record<string, unknown>): string {
  const content = result["content"];
  if (!Array.isArray(content)) return "";
  const parts: string[] = [];
  for (const block of content) {
    if (typeof block === "object" && block !== null && typeof (block as Record<string, unknown>)["text"] === "string") {
      parts.push((block as Record<string, string>)["text"] as string);
    }
  }
  return text(parts.join("\n"));
}

function failureMessage(result: Record<string, unknown>): string {
  const error = result["error"];
  if (typeof error === "object" && error !== null) {
    const message = (error as Record<string, unknown>)["message"];
    if (typeof message === "string") return text(message);
  }
  return "";
}

function demoToolName(config: BridgeConfig): string | undefined {
  if (typeof config.demoTool === "string" && config.demoTool.length > 0) return config.demoTool;
  return config.demoTool === true ? DEFAULT_DEMO_TOOL : undefined;
}

function demoTool(toolName: string): Record<string, unknown> {
  const schema = {
    type: "object",
    properties: { text: { type: "string", description: "text to echo back" } },
    required: ["text"],
    additionalProperties: false,
  };
  return {
    name: toolName,
    description: "Echo the given text back unchanged (tenon-bridge pipeline probe).",
    parameters: schema,
    output: {
      schema,
      render: (_args: unknown, value: unknown) => [
        { type: "text", text: text((value as Record<string, unknown>)["text"]) },
      ],
    },
    execute: (args: unknown) => Promise.resolve({ text: text((args as Record<string, unknown>)?.["text"] ?? "") }),
  };
}

function buildHandlers(host: HostContext, mirrors: () => MirrorInfo[]): Record<string, Handler> {
  let calls = 0;
  return {
    ping: () => "pong",
    pid: () => process.pid,
    mirrors: () => mirrors(),
    "tools.list": () =>
      service<ToolsService>(host, "tools")
        .schemas()
        .map((schema) => ({ name: schema.name, description: text(schema.description ?? "") })),
    "tools.execute": async (tool: unknown, input: unknown) => {
      calls += 1;
      const controller = new AbortController();
      const result = await service<ToolsService>(host, "tools").execute({
        callId: `tenon-${calls}`,
        name: String(tool),
        arguments: input ?? {},
        signal: controller.signal,
      });
      const isError = result["isError"] === true;
      return {
        ok: !isError,
        isError,
        content: blocksToText(result),
        error: failureMessage(result),
        value: isError ? null : (result["value"] ?? null),
      };
    },
    "sessions.list": () => service<SessionsService>(host, "sessions").list().map((session) => ({ id: text(session.id) })),
    "sessions.create": () => ({ id: text(service<SessionsService>(host, "sessions").create().id) }),
    "agents.list": () =>
      service<AgentsService>(host, "agents")
        .list()
        .map((agent) => ({ id: text(agent["id"]), session: text((agent["session"] as Record<string, unknown> | undefined)?.["id"]) })),
  };
}

function selectHandlers(all: Record<string, Handler>, methods: ServiceSpec["methods"]): Record<string, Handler> {
  if (methods === undefined) return all;
  const out: Record<string, Handler> = {};
  const pairs = Array.isArray(methods) ? methods.map((entry) => [entry, entry] as const) : Object.entries(methods);
  for (const [exposed, internal] of pairs) {
    const handler = all[internal];
    if (handler === undefined) {
      log(`tenon-bridge: no handler named "${internal}", skipped`);
      continue;
    }
    out[exposed] = handler;
  }
  return out;
}

export function apply(host: HostContext, config: BridgeConfig = {}): void {
  if (!wireReady()) {
    log("tenon-bridge: fd 3/4 are not open, staying inert (dsh is running outside tenon)");
    return;
  }
  const plugin = new Plugin({ inject: [] });
  const logger: Logger = (line) => plugin.log(line);
  const disposers: Disposer[] = [];
  let mirrors: Mirrors | undefined;

  plugin.onLoad(async (loaded) => {
    const merged: BridgeConfig = { ...config, ...(loaded as BridgeConfig) };
    await (host.get("loader") as { await?: () => Promise<unknown> } | undefined)?.await?.();
    const tool = demoToolName(merged);
    if (tool !== undefined) {
      disposers.push(service<ToolsService>(host, "tools").register(demoTool(tool)));
    }
    mirrors = installMirrors(host, plugin, merged.events ?? [], logger);
    const handlers = buildHandlers(host, () => mirrors?.list() ?? []);
    const services = merged.services ?? [{ name: DEFAULT_SERVICE }];
    for (const service of services) {
      const exposed = typeof service?.name === "string" && service.name.length > 0 ? service.name : DEFAULT_SERVICE;
      plugin.provide(exposed, selectHandlers(handlers, service?.methods));
      disposers.push(() => plugin.unprovide(exposed));
    }
    logger(`tenon-bridge: active, ${mirrors.list().length} mirrors, services ${services.length}`);
    return "ok";
  });

  plugin.onUnload(() => {
    mirrors?.dispose();
    while (disposers.length > 0) {
      const dispose = disposers.pop();
      try {
        dispose?.();
      } catch (error) {
        logger(`tenon-bridge: disposal failed: ${reason(error)}`);
      }
    }
  });

  try {
    plugin.run();
  } catch (error) {
    log(`tenon-bridge: cannot open the tenon wire: ${reason(error)}`);
  }
}
