# tenon-bus — the message fabric

RFC P4 sections 2-4: one envelope for every event, log, metric and status frame
from every language, and a `Hub` that fans it out to subscribers with
server-side filtering fast enough for a desktop-latency UI.

```
publish(env) --> Hub --> per-subscriber ring --> subscribe(filter, opts)
                  |
                  +-- durable? --> writer task (5 ms group commit) --> Durable log
```

## The envelope

`Envelope` is RFC section 2 verbatim: a closed core that drives
storage/delivery/visibility and an open `tags` + `payload` that policy never
reads.

| Field | Meaning |
|---|---|
| `topic` | one routing key, `ns/name`, hierarchy + glob subscribe |
| `ts`, `host`, `env`, `src` | origin (host for federation, src = component) |
| `level` | trace \| debug \| info \| warn \| error |
| `durable` | written to the log before publish returns; `event_id` dedup |
| `model_visible` | subject to the session-log law (implies `durable`) |
| `ttl_s?` | expiry for delivery and storage |
| `session?`, `step?` | correlation |
| `event_id` | ULID, idempotency and ordering key |
| `tags` | open map; the `key` tag is the `latest_only` compaction key |
| `payload` | JSON; bulk data goes to a blob and is referenced by hash |

`normalize()` runs at the door of every publish: `model_visible` implies
`durable`, a missing `event_id`/`ts` is filled.

## The hub

- **Lock-free publish.** The subscriber list lives in an `ArcSwap`; publish
  loads a snapshot and pushes an `Arc<Published>` (encoded once) into each
  matching ring. subscribe/unsubscribe take a mutex (cold path).
- **Per-subscriber ring.** Bounded; drop-oldest for non-durable envelopes when
  full, never-drop for durable ones (they replay from the log). `latest_only`
  keeps the last envelope per `(topic, key)` for status/metrics topics.
- **Coalescing.** `coalesce_ms` batches a burst into one `recv()` (the UI uses
  16 ms).
- **Durable path.** A single writer task batches durable envelopes with a 5 ms
  group-commit tick; `publish` of a durable envelope resolves after its batch is
  persisted. Non-durable envelopes never reach the writer — memory fan-out only.
- **Replay.** `subscribe{since_offset}` loads the durable log after that offset
  into the ring before any live envelope: the reconnect/sync protocol.

Durability is the host's `Durable` trait (`append_batch`, `since`, `head`); the
hub never sees SQLite. Env-scoping (RFC 8d.2) is a `Filter` the host pins to a
caller's env before subscribe — the hub only matches filters.

## The tracing layer

`BusLayer` turns every `tracing` event into an envelope: `info!(topic =
"worker/step", env = "root", n = 7)` becomes an envelope whose fields are the
payload and whose `topic`/`env`/`session`/`durable`/`model_visible` fields are
the closed core. Importing the crate is a Rust component's messaging ability.

## Files

`envelope.rs` (envelope + ULID), `filter.rs` (glob + conjunctive match),
`ring.rs` (ring + `Subscription`), `hub.rs` (Hub + writer + `Durable`),
`layer.rs` (tracing bridge), `lib.rs`. All under 600 lines.
