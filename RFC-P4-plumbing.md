# RFC P4 — Plumbing: bus, kv, blob, query (v1)

Author: Fable. 2026-08-19. Status: v1.1 (AGY review folded). P3 is the foundation and walls; P4 is the
plumbing and wiring: one message fabric and one state facade that every component uses, with SQLite
demoted to an implementation detail behind them. Engram (memory) and the navigator are P5+ consumers
of these pipes, not part of them.

## 0. Goals and non-goals

Goals: one envelope for events/logs/metrics/status from every language; pub/sub with server-side
filtering fast enough for a desktop-latency UI; an etcd-lite kv; content-addressed blobs; a typed
query facade (numeric scan/aggregate + full-text now, vector-ready) whose speed does not degrade as
volume grows; the fewest host files; cluster-ready seams at zero single-host cost; smaller LoC by
deleting the ad-hoc RPC families P3 accumulated.

Non-goals (explicitly): egress/network policy is NOT defined here (backend-specific, decided per sandbox impl later); file watching is the guest kernel's job (inotify inside the VM); application push (email/APNs) is left to cloud services or AI-built code inside the VM — the trigger plugin's http_post is the only outbound hook. MCP is an adapter plugin (P4.7), never a facade; no broker process (no Kafka/NATS/Redis/zenoh now); no cross-network
exactly-once; no Raft yet; no SQL exposure; no embedding model (interface only).

## 1. Lessons folded in

- DSH survey: FTS5 as a rebuildable derived read model (version-gated, drop-and-rebuild) is the
  right pattern; DSH lacks vector search, cross-session memory, and any DB retention — those are our
  差异化. We adopt "log = truth, indexes = disposable derivations" as law.
- Our own P3: everything already flows through base RPC; P4 is a收口, not a rewrite.

## 2. The envelope (closed core + open tags)

```
Envelope {
  topic: "ns/name"        one routing key, hierarchy + glob subscribe; reserved ns: session/
                          internal/ base/ budget/ approval/ guardian/ upgrade/ worker/
  ts, host, env, src      origin (host for federation later; src = component name)
  level                   trace|debug|info|warn|error
  durable: bool           true -> written to the log before publish returns (at-least-once,
                          event_id dedup -> effectively-once in the store)
  model_visible: bool     true -> subject to the session-log law (implies durable)
  ttl_s?: u32             expiry for delivery and storage (None = table retention policy)
  session?, step?         correlation
  event_id                ulid, idempotency
  tags: {k: v}            open, free; policy never depends on tags
  payload                 JSON; bulk data goes to blob and is referenced by hash
}
```
Closed fields drive storage/delivery/visibility decisions; tags/topics are open (anyone declares).
`topic.declare` (discovery/UI columns) is deferred until the UI needs it; a CI lint lists topics
found in code, never fails. Envelope size cap = wire frame cap.

## 3. The four facades (the only way anything touches state)

| Facade | API | Backing |
|---|---|---|
| bus | `publish(env)`, `subscribe{filter{topics glob, levels, env, session, tags}, since_offset?, coalesce_ms?, latest_only?}` | Hub in base + `events` table for durable topics |
| kv | `get/set{durable?}/del/cas/incr/expire/lease/keep_alive/range(prefix)/watch(prefix, since_rev)`; global monotonic `revision` | memory map + SQLite for durable keys; watch events ride the bus |
| kv: ControlLease | `/ctl/<env>` lease: the holder may send input, other attached terminals render read-only, idle 30 s auto-release, explicit takeover allowed | first consumer of lease; multi-terminal co-display |
| blob | `put(bytes|path) -> sha256`, `get`, `open(offset,len)`, `stat` | BLOB rows in state files; >threshold spills to per-env files later (deferred) |
| query | `text{q, filter, topk}`, `scan{filter, aggregate}`, `vector{emb, topk}` (stub), typed JSON — no SQL | hot: SQLite indexes + FTS5; warm: derived segments (P4.3) |

Everything else P3 exposed (`events.*`, `episodes.*`, `tool_results.*`, `approvals.*` reads,
`subscribe`) becomes an alias over these four and is deleted after migration. episodes/tool_results
are durable topics + query; approvals stay an RPC for the decision path but their listing/history is
query; guardian probes read the bus.

## 4. Hub design (desktop-latency budget)

- Publish path lock-free: encode once, fan out `Arc<bytes>`; per-subscriber bounded ring
  (drop-oldest for non-durable, never-drop via log replay for durable); topic index in `ArcSwap`
  prefix tree; subscribe/unsubscribe take a mutex (cold path).
- Durable topics: single writer task batches into the state file; publish returns after the batch
  fsync tick (group commit, 5 ms default; decided). Non-durable envelopes (token chunks, audio,
  telemetry, thinking fragments) NEVER touch disk or the writer task: lock-free memory fan-out only,
  target < 0.1 ms.
- `latest_only` (topic+key compaction) for status/metrics topics; `coalesce_ms` batches frames per
  subscriber (UI uses 16 ms).
- Rust components integrate via a `tracing` Layer (import = instant messaging ability: `info!`,
  `event!` become envelopes). Elixir: Logger handler + `:telemetry` via Link. Python/other: the
  wire frame `t:"ev"`.
- Budgets (acceptance): publish -> same-host subscriber model update p99 < 1 ms; 100k msg/s
  background with a stable 60 fps coalesced UI; zero durable loss under `kill -9` (replay from log).

## 5. Query layer (no degradation as volume grows)

- Hot (truth): state files keep only the recent window fully indexed (config, default 14 days):
  composite indexes on (topic, ts), (session, seq); FTS5 over text payload fields of durable topics.
- Warm (derived, disposable, rebuildable from the log — DSH pattern generalized): a background
  compactor rolls aged events into immutable segments: Parquet (columnar, per env+month) scanned by
  minimal pure-Rust `parquet`/`arrow-rs` (feature-gated; DuckDB rejected — C++, 30-50 MB; Polars
  rejected as similarly heavy). Hard gate: release binary stays < 80 MB, else fall back to SQLite
  partition tables as segments. Tantivy segments for BM25 text; vector search is a stub (engine and
  embeddings belong to P5 memory). Query fans out to relevant segments (time/session
  pruning) in parallel and merges — single-host shard query. Version-gated: wrong version -> drop
  and rebuild.
- Acceptance at 10M events: text < 50 ms, scan/aggregate < 200 ms, flat month-over-month (vector budgets belong to P5).
- P4 ships hot layer + the segment interfaces; the warm compactor is P4.3 and may land after P5
  starts (Engram consumes the `query` facade, not the engine).

## 6. Host files (fewer than P3)

`config.yml`, `state.sqlite` (barebone), `state-<env>.sqlite` (per env), optional
`workspace-<env>.img`. Warm segments live in a `derived/` dir that can be deleted at any time
(rebuildable, excluded from LKG). No other files; blobs stay inside the state files (spill file
support deferred until a real >1 GB case appears).

## 7. Cluster-ready seams (zero cost now)

kv carries revision + lease + watch (etcd semantics) so membership/placement/scheduler are later
plugins: hosts hold TTL leases under `/hosts/`, desired env state under `/envs/<id>/spec` with a
reconciliation loop (Kubernetes pattern) — the future task manager, orthogonal to the navigator.
Envelope `host` is the origin host (loop prevention when bridging); a future bridge is a plain
plugin built on subscribe+publish, no reserved RPC; ids host-prefixed;
`runtime.spawn{placement}` placeholder; kv durable layer is written as append-log + snapshot so Raft
(openraft) can replace it without API change. Federation itself (SWIM/Raft/mTLS/topic bridge) is P6.

## 8. Changes to P0-P3 (updates, all shrinking or additive)

- RFC-P3 section 9 storage: superseded by this facade view; storage crate becomes private to base.
- rs/base envrpc/rpc: the seven record RPC families collapse into bus/kv/blob/query aliases; delete
  after migration (target: net negative LoC in base).
- harness/worker/guardian: emit through the tracing Layer / Link telemetry instead of bespoke event
  appends; `worker/step`, `budget.*`, `guardian.*`, `upgrade.phase` become plain topics.
- UI (rs/ui carriers): 100% subscription-driven (decided) with coalesce + latest_only; the status
  RPC remains only for `tenon status` one-shots. Multi-terminal attach uses ControlLease.
- serve --http hardening, IN P4 scope (feature-gated, off by default): `tenon serve --https
  [--cert PEM --key PEM]` via rustls; no cert given -> rcgen generates an in-memory self-signed cert
  (dev mode, fingerprint printed); auth = bearer token (`TENON_AUTH_TOKEN` or `--auth-token`,
  checked on every HTTP/WS/SSE request; constant-time compare); production guidance = reverse proxy
  (Caddy/Tailscale) or JWT header pass-through (documented seam only).
- kernel, loader, sandbox, sdk wire: unchanged. Kernel stays frozen.

## 8b. Media and mobile seams (documented now, plugins later)

Control plane on the bus, media plane by handle — streaming is the streaming case of the existing
bulk-data rule. Signaling topics (`media/offer|accept|stop`, non-durable fast path); negotiated media
runs plugin-to-plugin: same host = UDS/shared-memory + fd passing (already used for PTY), remote or
browser = WebRTC (RTP/Opus/VP8) or binary WS. A `media` plugin (or the Bruno sidecar) owns
devices/codecs and registers capabilities in kv; native-audio models connect the media channel
directly to the provider endpoint while the harness only initiates and logs events. The frame cap
keeps streams off the bus by construction.

Mobile: the App is just another subscriber — same RPC + SSE or the P4.4 WS carrier; log = truth
+ `since_offset` replay is the sync protocol (reconnect pulls the delta, nothing durable is lost);
`latest_only` topics for light state; one env per user/device for tenancy, budgets and approvals; a
`notify` plugin (APNs/FCM) subscribes to the bus; thin device SDKs (~200 lines Swift/Kotlin) mirror
sdk/py/ts/rs. All L2 plugins; no architecture change.

## 8c. App platform: ingress, and the data-layering rule

Evaluation (2026-08-19): with P4 the facades cover ~70% of a BaaS (state=kv, files=blob,
realtime=bus with offset resync, search=query, server logic=long-lived python/node inside the VM,
audit/undo=log+snapshots, tenancy/quotas=env tree+budgets, TLS/auth/WS=P4.4). Two gaps closed here:

Ingress (route + port mapping, ~150 lines inside the http feature):
- Registration: an app inside the sandbox calls gateway svc `ingress.register{name, port, public?}`;
  the worker/base validates (name unique per host, env owns it, count/quota from config,
  non-public apps inherit the bearer token) and writes `/ingress/<name> -> {env, addr}` into kv
  (lease-backed: app dies -> route expires). `ingress.list` and CLI `tenon ingress` read kv.
- Routing: `serve` proxies `/app/<name>/*` -> the sandbox address (oci: mapped port on 127.0.0.1;
  krun: TSI-mapped port; landlock: localhost port), strips the prefix, adds `X-Tenon-App`,
  `X-Tenon-Env`; WS upgrade passes through (same carrier); public apps skip the token, everything
  else requires it. No subdomains, no per-app TLS, no load balancing — one host, one proxy route.
- Safety: routes only into that env's own sandbox; body size and connection caps from config;
  `ingress.register` can be listed in `gated_tools` to require human approval.

Data layering rule (the SQL answer): one app, one file — an app's relational data lives in ITS OWN
sqlite file inside its workspace (python/node have sqlite natively; git-snap snapshots it, giving
DB time-travel for free). The platform never offers a shared SQL server; cross-app/shared state goes
through kv/bus/blob only (permissioned, audited, evented). kv/bus are coordination primitives, not a
query engine for app schemas; the query facade serves the platform's own log/memory. If a managed DB
is ever needed: an additive `db.open(name)` facade (per-env libSQL file + quota + snapshot-backed
backups) — not built now.

## 9. Plan

| Step | Deliverable | Gate |
|---|---|---|
| P4.0 | `rs/bus` (envelope, Hub, tracing Layer, `t:"ev"` frames), kv facade (memory + durable + lease/watch/revision), blob facade over existing blobs, RPCs `bus.publish/subscribe`, `kv.*`, `blob.*`; timer service (`timer.set{topic, cron|after, payload}` -> envelope on schedule, one timer wheel in the Hub) | budgets in section 4 measured by a bench test; durable replay after kill -9; a cron timer fires on schedule and survives base restart (kv-stored); existing suites green |
| P4.1 | migration: harness/worker/guardian/UI/CLI onto the facades; delete legacy RPC families; Elixir Logger/telemetry bridge | net LoC reduction in base recorded; all suites green; UI runs on subscribe (no polls) |
| P4.2 | query hot layer: typed `query.text/scan`, FTS5 over durable topics, composite indexes, retention window config | text < 10 ms and scan < 100 ms at 1M events (bench in tests) |
| P4.3 | warm segments: compactor to Parquet + Tantivy (vector stub), fan-out merge, version-gated rebuild, `derived/` lifecycle | 10M-event budgets of section 5; rebuild-from-log test |
| P4.4 | `--https` (rustls + rcgen dev self-signed) + bearer auth on serve, feature-gated; secrets facade (`secret.get(name)`: values live only in base config/env refs, per-env grants, never in envelopes — the bus redacts known secret values from payloads); WebSocket as the 5th wire carrier (tokio-tungstenite, same feature): `/ws` on serve (RPC + subscribe over WS text frames, binary frames reserved for media chunks) and WS accept on the gateway (`TENON_GATEWAY` gains `ws:`; each connection mounts as a fiber — lets browser extensions such as the vibe-browse Chrome bridge register as plugins without a python side-server) | curl over https works; no token = 401; a WS client subscribes and receives coalesced envelopes; a WS client speaks hello/provide and its svc answers through the kernel; feature off = binary unchanged |
| P4.5 | ingress (section 8c): `ingress.register/list`, kv lease routes, `/app/<name>/*` proxy incl. WS pass-through, quotas | an app started inside the sandbox registers and is reachable through https with the token; killing the app expires the route; a second env cannot claim the same name |
| P4.7 | triggers: `trigger.set{filter, action, ttl?}` plugin (kv-stored; actions publish / http_post with retry+budget / prompt{env}; hop counter in the envelope prevents loops; sensitive actions gateable) + inbound webhook route `POST /hook/<topic>` on serve (token -> publish); MCP bridge plugin, both directions: client (spawn/connect an MCP server, `tools/list` registered into the tools bus under single authority, `tools/call` forwarded; guard/budget/approval hooks apply to bridged tools) and server (expose worker + management tools over MCP stdio and streamable HTTP on serve, token-authenticated, gated tools go through approvals) | an external MCP server's tool is callable by the model through our loop and is denied by a guard hook; Claude Code connects to Tenon-as-MCP-server and runs bash in the sandbox |
| P4.8 | `tenon doctor` (self-diagnostics: toolchain, sandbox backends, ports, state integrity); docs + REVIEW-P4 (perf tables incl. UI latency), NOTES update | all gates; secret scan; no leftover containers |

LoC estimate: bus 0.8-1k, kv 0.4k, blob facade 0.1k, query hot 0.5k, warm compactor 0.8-1k, minus
~0.5-0.8k deleted legacy RPC/plumbing = net ~+2k Rust for the whole of P4, tests separate. Crates:
`rs/bus` new; kv/blob/query live in base + storage (no new processes, no new deps beyond tantivy/
parquet in P4.3, feature-gated).

## 10. Decisions (AGY review, 2026-08-19)

1. Group commit 5 ms default; durable:false never persists (zero-fsync memory path).
2. No DuckDB, no Polars; pure-Rust parquet/arrow minimal, binary < 80 MB hard gate, SQLite-partition
   fallback.
3. Vector/embedding engine belongs to P5 memory; P4 keeps only the query.vector stub.
4. UI fully subscription-driven; ControlLease added to kv.
5. TLS/auth/WS moved INTO P4 scope (P4.4), feature-gated, off by default; production SSO stays a documented seam.
