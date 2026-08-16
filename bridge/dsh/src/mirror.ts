import type { Plugin } from "#sdk";

const MAX_DEPTH = 4;
const MAX_ITEMS = 64;
const MAX_STRING = 4096;
const FRAME_HEADROOM = 0.75;
const REASON_TOKEN = "$reason";

const DEFAULT_DENY: Readonly<Record<string, unknown>> = { kind: "deny", reason: REASON_TOKEN };

export type Disposer = () => void;
export type HookMode = "emit" | "call";
export type Logger = (line: string) => void;

export interface HostContext {
  on(event: string, listener: (...args: never[]) => unknown, prepend?: boolean): Disposer;
  get(name: string): unknown;
}

export interface EventSpec {
  name: string;
  mode?: HookMode;
  pick?: string[];
  prepend?: boolean;
  deny?: Record<string, unknown>;
}

export interface MirrorInfo {
  name: string;
  mode: HookMode;
  pick: string[];
}

export interface Mirrors {
  list(): MirrorInfo[];
  dispose(): void;
}

export function reason(error: unknown): string {
  if (error instanceof Error) return error.message || error.name;
  return String(error);
}

function projectRecord(source: object, keys: readonly string[], depth: number, seen: Set<object>): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const key of keys) {
    let raw: unknown;
    try {
      raw = (source as Record<string, unknown>)[key];
    } catch {
      continue;
    }
    const value = projectValue(raw, depth + 1, seen);
    if (value !== undefined) out[key] = value;
  }
  return out;
}

function projectValue(value: unknown, depth: number, seen: Set<object>): unknown {
  if (value === null) return null;
  switch (typeof value) {
    case "string":
      return value.length > MAX_STRING ? `${value.slice(0, MAX_STRING)}...` : value;
    case "number":
      return Number.isFinite(value) ? value : String(value);
    case "boolean":
      return value;
    case "bigint":
      return String(value);
    case "object":
      break;
    default:
      return undefined;
  }
  const object = value as object;
  if (seen.has(object)) return "[circular]";
  if (depth >= MAX_DEPTH) return "[depth]";
  seen.add(object);
  try {
    if (Array.isArray(object)) {
      const items: unknown[] = [];
      for (const item of object.slice(0, MAX_ITEMS)) {
        const projected = projectValue(item, depth + 1, seen);
        items.push(projected === undefined ? null : projected);
      }
      return items;
    }
    let keys: string[] = [];
    try {
      keys = Object.keys(object).slice(0, MAX_ITEMS);
    } catch {
      return "[opaque]";
    }
    return projectRecord(object, keys, depth, seen);
  } finally {
    seen.delete(object);
  }
}

export function projectArgs(args: readonly unknown[], pick: readonly string[] | undefined): unknown[] {
  const seen = new Set<object>();
  return args.map((arg) => {
    if (pick !== undefined && pick.length > 0 && typeof arg === "object" && arg !== null && !Array.isArray(arg)) {
      return projectRecord(arg, pick, 0, seen);
    }
    const projected = projectValue(arg, 0, seen);
    return projected === undefined ? null : projected;
  });
}

function fit(args: unknown[], cap: number): unknown[] {
  let size = cap + 1;
  try {
    size = Buffer.byteLength(JSON.stringify(args) ?? "");
  } catch {
    size = cap + 1;
  }
  if (size <= cap) return args;
  return args.map(() => ({ truncated: true, bytes: size }));
}

export function mergeBack(
  args: readonly unknown[],
  reply: readonly unknown[],
  pick: readonly string[] | undefined,
  log: Logger,
): void {
  if (pick === undefined || pick.length === 0) return;
  for (let index = 0; index < args.length; index += 1) {
    const target = args[index];
    const source = reply[index];
    if (typeof target !== "object" || target === null) continue;
    if (typeof source !== "object" || source === null || Array.isArray(source)) continue;
    for (const key of pick) {
      if (!Object.hasOwn(source, key)) continue;
      try {
        (target as Record<string, unknown>)[key] = (source as Record<string, unknown>)[key];
      } catch (error) {
        log(`tenon-bridge: ${key} is not writable: ${reason(error)}`);
      }
    }
  }
}

export function denialReason(reply: unknown): string {
  if (typeof reply === "string" && reply.length > 0) return reply;
  if (typeof reply === "object" && reply !== null) {
    for (const key of ["deny", "reason", "error", "message"]) {
      const value = (reply as Record<string, unknown>)[key];
      if (typeof value === "string" && value.length > 0) return value;
    }
  }
  return "denied by tenon";
}

function substitute(template: unknown, text: string): unknown {
  if (typeof template === "string") return template.split(REASON_TOKEN).join(text);
  if (Array.isArray(template)) return template.map((item) => substitute(item, text));
  if (typeof template === "object" && template !== null) {
    const out: Record<string, unknown> = {};
    for (const [key, value] of Object.entries(template)) out[key] = substitute(value, text);
    return out;
  }
  return template;
}

function emitMirror(host: HostContext, plugin: Plugin, spec: EventSpec, log: Logger): Disposer {
  const cap = Math.floor(plugin.maxFrame * FRAME_HEADROOM);
  return host.on(
    spec.name,
    ((...args: unknown[]): void => {
      try {
        plugin.emit(spec.name, fit(projectArgs(args, spec.pick), cap));
      } catch (error) {
        log(`tenon-bridge: emit ${spec.name} failed: ${reason(error)}`);
      }
    }) as (...args: never[]) => unknown,
    spec.prepend === true,
  );
}

function callMirror(host: HostContext, plugin: Plugin, spec: EventSpec, log: Logger): Disposer {
  const cap = Math.floor(plugin.maxFrame * FRAME_HEADROOM);
  return host.on(
    spec.name,
    (async (...received: unknown[]): Promise<unknown> => {
      const next = received[received.length - 1] as () => Promise<unknown>;
      const args = received.slice(0, -1);
      if (typeof next !== "function") {
        log(`tenon-bridge: ${spec.name} is not a waterfall event, mirror skipped`);
        return undefined;
      }
      let reply: unknown;
      try {
        reply = await plugin.call(spec.name, fit(projectArgs(args, spec.pick), cap));
      } catch (error) {
        log(`tenon-bridge: call ${spec.name} failed: ${reason(error)}`);
        return next();
      }
      if (Array.isArray(reply) && reply.length === args.length) {
        mergeBack(args, reply, spec.pick, log);
        return next();
      }
      return substitute(spec.deny ?? DEFAULT_DENY, denialReason(reply));
    }) as (...args: never[]) => unknown,
    spec.prepend === true,
  );
}

export function installMirrors(host: HostContext, plugin: Plugin, specs: readonly EventSpec[], log: Logger): Mirrors {
  const disposers: Disposer[] = [];
  const info: MirrorInfo[] = [];
  for (const spec of specs) {
    if (typeof spec?.name !== "string" || spec.name.length === 0) {
      log("tenon-bridge: skipping an event manifest row without a name");
      continue;
    }
    const mode: HookMode = spec.mode === "call" ? "call" : "emit";
    try {
      disposers.push(mode === "call" ? callMirror(host, plugin, spec, log) : emitMirror(host, plugin, spec, log));
      info.push({ name: spec.name, mode, pick: [...(spec.pick ?? [])] });
    } catch (error) {
      log(`tenon-bridge: cannot mirror ${spec.name}: ${reason(error)}`);
    }
  }
  return {
    list: () => info.map((entry) => ({ ...entry, pick: [...entry.pick] })),
    dispose: () => {
      while (disposers.length > 0) {
        const dispose = disposers.pop();
        try {
          dispose?.();
        } catch (error) {
          log(`tenon-bridge: mirror disposal failed: ${reason(error)}`);
        }
      }
    },
  };
}
