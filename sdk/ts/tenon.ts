import { writeSync } from "node:fs";
import { Socket } from "node:net";

const WIRE_IN_FD = 3;
const WIRE_OUT_FD = 4;
const DEFAULT_MAX_FRAME = 1048576;
const DEFAULT_DEADLINE_MS = 30000;

export type HookMode = "emit" | "call";
export type Next = (args?: unknown[]) => Promise<unknown>;
export type EmitHandler = (args: unknown[]) => void | Promise<void>;
export type CallHandler = (args: unknown[], next: Next) => unknown;
export type Handler = EmitHandler | CallHandler;
export type Methods = Record<string, (...args: any[]) => unknown>;

export interface HookOptions {
  mode?: HookMode;
  prepend?: boolean;
  arity?: number;
}

export interface PluginOptions {
  inject?: string[];
}

type Frame = Record<string, any>;

export class TenonError extends Error {}

export class FrameTooLarge extends TenonError {
  constructor(readonly size: number, readonly cap: number) {
    super("frame_too_large");
  }
}

function envInt(name: string, fallback: number): number {
  const value = Number.parseInt(process.env[name] ?? "", 10);
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

function reason(error: unknown): string {
  if (error instanceof Error) return error.message || error.name;
  return String(error);
}

export class Plugin {
  readonly inject: string[];
  readonly maxFrame: number;
  readonly deadlineMs: number;
  config: Record<string, unknown> = {};

  #hooks = new Map<number, { mode: HookMode; handler: Handler }>();
  #services = new Map<string, Methods>();
  #pending = new Map<string, { resolve: (v: unknown) => void; reject: (e: unknown) => void }>();
  #deferred: Frame[] = [];
  #buffer: Buffer<ArrayBufferLike> = Buffer.alloc(0);
  #input: Socket | null = null;
  #seq = 0;
  #active = false;
  #stopped = false;
  #loadHandler: ((config: Record<string, unknown>) => unknown) | null = null;
  #unloadHandler: (() => unknown) | null = null;

  constructor(options: PluginOptions = {}) {
    this.inject = [...(options.inject ?? [])];
    this.maxFrame = envInt("TENON_MAX_FRAME", DEFAULT_MAX_FRAME);
    this.deadlineMs = envInt("TENON_KERNEL_DEADLINE", DEFAULT_DEADLINE_MS);
  }

  log(message: string): void {
    process.stderr.write(`${message}\n`);
  }

  on(event: string, handler: Handler, options: HookOptions = {}): number {
    const mode = options.mode ?? "emit";
    const hook = this.#alloc();
    this.#hooks.set(hook, { mode, handler });
    this.#register({
      t: "on",
      hook,
      event,
      arity: options.arity ?? 1,
      mode,
      prepend: options.prepend === true,
    });
    return hook;
  }

  off(hook: number): void {
    this.#hooks.delete(hook);
    this.#register({ t: "off", hook });
  }

  provide(name: string, methods: Methods): void {
    this.#services.set(name, { ...methods });
    this.#register({ t: "provide", name });
  }

  unprovide(name: string): void {
    this.#services.delete(name);
    this.#register({ t: "unprovide", name });
  }

  emit(event: string, args: unknown[] = []): void {
    this.#send({ t: "emit", event, args: [...args] });
  }

  call(event: string, args: unknown[] = []): Promise<unknown> {
    const id = this.#alloc();
    this.#send({ t: "call", id, event, args: [...args] });
    return this.#park(`rep:${id}`);
  }

  svc(name: string, method: string, args: unknown[] = []): Promise<unknown> {
    const id = this.#alloc();
    this.#send({ t: "svc", id, name, method, args: [...args] });
    return this.#park(`rep:${id}`);
  }

  onLoad(handler: (config: Record<string, unknown>) => unknown): void {
    this.#loadHandler = handler;
  }

  onUnload(handler: () => unknown): void {
    this.#unloadHandler = handler;
  }

  run(): void {
    this.#send({ t: "hello", inject: this.inject });
    const input = new Socket({ fd: WIRE_IN_FD, readable: true, writable: false });
    this.#input = input;
    input.on("data", (chunk: Buffer) => this.#feed(chunk));
    input.on("end", () => void this.#shutdown());
    input.on("error", (error) => {
      this.log(`tenon: wire read failed: ${reason(error)}`);
      void this.#shutdown();
    });
  }

  #alloc(): number {
    this.#seq += 1;
    return this.#seq;
  }

  #register(frame: Frame): void {
    if (this.#active) this.#send(frame);
    else this.#deferred.push(frame);
  }

  #send(frame: Frame): void {
    const body = Buffer.from(JSON.stringify(frame), "utf8");
    if (body.length > this.maxFrame) throw new FrameTooLarge(body.length, this.maxFrame);
    const packet = Buffer.alloc(4 + body.length);
    packet.writeUInt32BE(body.length, 0);
    body.copy(packet, 4);
    let written = 0;
    while (written < packet.length) {
      try {
        written += writeSync(WIRE_OUT_FD, packet, written, packet.length - written);
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "EAGAIN") throw error;
      }
    }
  }

  #feed(chunk: Buffer): void {
    this.#buffer = this.#buffer.length === 0 ? chunk : Buffer.concat([this.#buffer, chunk]);
    for (;;) {
      if (this.#buffer.length < 4) return;
      const size = this.#buffer.readUInt32BE(0);
      if (this.#buffer.length < 4 + size) return;
      const body = this.#buffer.subarray(4, 4 + size);
      this.#buffer = this.#buffer.subarray(4 + size);
      void this.#dispatch(JSON.parse(body.toString("utf8")) as Frame);
    }
  }

  // Frames are dispatched without awaiting the previous one, so a handler that
  // awaits next/svc keeps the loop free to deliver the reply it is waiting for.
  async #dispatch(frame: Frame): Promise<void> {
    switch (frame.t) {
      case "hook":
        return this.#onHook(frame);
      case "svc":
        return this.#onSvc(frame);
      case "result":
        return this.#settle(`result:${frame.req}`, frame.result, undefined);
      case "rep":
        return this.#settle(`rep:${frame.id}`, frame.result, frame.error);
      case "load":
        return this.#onLoadFrame(frame);
      case "unload":
        return this.#shutdown();
      default:
        this.log(`tenon: ignoring frame ${String(frame.t)}`);
    }
  }

  #park(key: string): Promise<unknown> {
    return new Promise((resolve, reject) => this.#pending.set(key, { resolve, reject }));
  }

  #settle(key: string, result: unknown, error: unknown): void {
    const slot = this.#pending.get(key);
    if (!slot) return;
    this.#pending.delete(key);
    if (error === undefined || error === null) slot.resolve(result);
    else slot.reject(new TenonError(String(error)));
  }

  async #onLoadFrame(frame: Frame): Promise<void> {
    this.config = (frame.config ?? {}) as Record<string, unknown>;
    this.#active = true;
    for (const pending of this.#deferred) this.#send(pending);
    this.#deferred = [];
    await this.#guard(frame.req, async () => {
      if (this.#loadHandler) await this.#loadHandler(this.config);
      return "ok";
    });
  }

  async #onHook(frame: Frame): Promise<void> {
    const entry = this.#hooks.get(frame.hook);
    const args = (frame.args ?? []) as unknown[];
    if (frame.mode !== "call") {
      if (!entry) return;
      try {
        await (entry.handler as EmitHandler)(args);
      } catch (error) {
        this.log(`tenon: hook ${String(frame.event)} failed: ${reason(error)}`);
      }
      return;
    }
    if (!entry) {
      this.#fail(frame.req, `unknown hook ${String(frame.hook)}`);
      return;
    }
    await this.#guard(frame.req, () =>
      (entry.handler as CallHandler)(args, this.#nexter(frame.req)),
    );
  }

  #nexter(req: number): Next {
    return (args: unknown[] = []) => {
      this.#send({ t: "next", req, args: [...args], await: true });
      return this.#park(`result:${req}`);
    };
  }

  async #onSvc(frame: Frame): Promise<void> {
    const impl = this.#services.get(frame.name)?.[frame.method];
    if (!impl) {
      this.#fail(frame.req, `unknown method ${String(frame.method)}`);
      return;
    }
    const args = (frame.args ?? []) as unknown[];
    await this.#guard(frame.req, () => impl(...args));
  }

  async #guard(req: number | undefined, body: () => unknown): Promise<void> {
    let result: unknown;
    try {
      result = await body();
    } catch (error) {
      this.log(`tenon: request ${String(req)} failed: ${reason(error)}`);
      this.#fail(req, reason(error));
      return;
    }
    try {
      this.#send({ t: "rep", req, result });
    } catch (error) {
      if (!(error instanceof FrameTooLarge)) throw error;
      this.log(`tenon: reply of ${error.size} bytes over cap ${error.cap}`);
      this.#fail(req, "frame_too_large");
    }
  }

  #fail(req: number | undefined, message: string): void {
    if (req === undefined || req === null) return;
    try {
      this.#send({ t: "rep", req, error: message });
    } catch {
      this.#send({ t: "rep", req, error: "frame_too_large" });
    }
  }

  async #shutdown(): Promise<void> {
    if (this.#stopped) return;
    this.#stopped = true;
    try {
      if (this.#unloadHandler) await this.#unloadHandler();
    } catch (error) {
      this.log(`tenon: unload handler failed: ${reason(error)}`);
    }
    this.#input?.destroy();
    process.exit(0);
  }
}
