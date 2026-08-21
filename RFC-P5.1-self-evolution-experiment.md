# RFC P5.1 — first self-evolution experiment (throttled, offline, audited)

Author: Fable, 2026-08-21. Status: running. No Rust changes (per user). agy runs INSIDE the isolated
OCI sandbox (tenon-agy-box: /tenon read-only, /workspace read-write, host invisible, creds a copy in a
named volume). All artifacts land in /workspace (bind-mounted to host, crash-recoverable) and the
creds/history named volume (also host disk). A rogue agent cannot touch ~/workspace or ~/.gemini.

## Rate reality (measured)
One agy task made ~25 Gemini calls and already hit ~5 429s. Tenon's rate limiter only throttles TASK
STARTS, not agy's internal per-turn calls (agy->Google direct). So ban-avoidance = OUTER throttle:
serialize one account, big inter-task gaps, circuit-breaker on repeated 429s, daily cap. Not an
unattended tight loop. The scheduler (scripts, host-side, not Rust) enforces this.

## What agy CAN do this round (reachable)
- Explore /tenon deeply; replicate parts of the runtime in /workspace/replica (its own minimal
  kernel/loader/wire), documenting every decision.
- Enumerate what the design is MISSING, each as /workspace/proposals/<n>/ (problem, design, prototype,
  how-to-verify).
- Iterate on its own plugins/tests (self-improvement via deterministic feedback).
- A/B: same task twice, compare.
- Audit law: every change writes AUDIT.md / CHANGELOG in its run dir; all in /workspace.

## What it CANNOT do yet (needs Rust, paused)
Drive the real hotswap / change-protocol / blue-green on the LIVE runtime — that needs the gateway/MCP
control plane wired into agy's container (Rust). This round is replicate-and-document, not live
hotswap. Live hotswap is the next step once Rust is unpaused.

## Throttle (scheduler)
serial; inter-task gap >= 600s; per-task 429 count parsed from agy's log; >8 429s or a failed task ->
back off 2h; >=3 consecutive bad tasks -> STOP (write STOPPED). Daily cap 24 tasks. All logged to
/workspace/scheduler.log.

## Monitoring
Mechanical (always on): sandbox isolation + the throttle/circuit-breaker. Judgment: Fable ~1h wakeups,
Opus ~10min checks. Human: user reviews /workspace in 3-4h.
