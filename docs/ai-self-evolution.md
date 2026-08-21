# AI Self-Evolution Experiment 01

Can an off-the-shelf CLI coding agent, running autonomously and unsupervised,
read the Tenon microkernel, understand it, and improve it — without touching
the real source and without leaking anything? This is the record of the first
run.

The headline result: **an independent AI reading only the source found a real,
unpatched denial-of-service vulnerability in the kernel** (atom-table
exhaustion), plus a genuine spec-versus-implementation drift in the message
bus. Both were verified by hand against the code.

## Why do this

Tenon is a sub-1000-line Erlang microkernel for composing AI agent systems.
The central bet is *legibility*: keep the kernel small and clear enough that
both humans and machines can reason about it. This experiment tests that bet
directly — if a fresh AI can reconstruct the architecture from source and find
real problems, the design is legible in the way it claims to be.

## Design: isolation, throttle, audit

The agent runs unsupervised, so the design is built around three guarantees.

- **Isolation.** The agent runs inside an OCI sandbox container. Its only
  writable surface is a bind-mounted scratch workspace and its own credential
  volume. It cannot reach the host's files or the real kernel repository. Every
  proposed change to Rust or Erlang lands as a *sketch* in the workspace; it is
  never applied to the live kernel by the agent.
- **Throttle.** A scheduler runs tasks strictly serially, with a fixed gap
  between them, an exponential backoff on failure, a daily cap, and a circuit
  breaker that opens after repeated failures. This keeps an autonomous run from
  hammering the model provider.
- **Audit.** Every invocation records the exact prompt and a redacted log.
  Every produced artifact carries an `AUDIT.md` and a changelog. The scheduler
  journal records every task outcome. The full run is replayable:
  prompt to log to artifact to audit note, round by round.

## Method

- Agent: a standalone CLI coding agent, medium reasoning effort.
- Tasks, looped: replicate the kernel, find design gaps, iterate, extend.
- Nine rounds over roughly three and a half hours.

## Results

The provider rate-limited the account throughout (this is a property of the
account tier, not of Tenon). After a single early backoff, every task
completed successfully, and the rate-limit pressure eased as the quota window
rolled.

| Round | Task | Result | Rate-limit hits |
|-------|------|--------|-----------------|
| 1 | replicate | timeout (backoff) | 9 |
| 2 | gaps | ok | 7 |
| 3 | iterate | ok | 8 |
| 4 | extend | ok | 7 |
| 5 | replicate | ok | 5 |
| 6 | gaps | ok | 5 |
| 7 | iterate | ok | 4 |
| 8 | extend | ok | 6 |
| 9 | replicate | ok | 5 |

### What the agent produced

- A working re-implementation of the kernel in Python (eight modules) with a
  test suite — evidence it understood the design well enough to rebuild it.
- Five code-grounded improvement proposals, each with a problem statement, a
  design, a verification plan, and a code sketch.
- A self-tested prototype plugin (it actually ran its own plugin in the
  sandbox).

## Findings verified against the source

Two claims were checked by hand against the live tree, not taken on the
agent's word.

### 1. Atom-table exhaustion — confirmed, a real vulnerability

The kernel converts wire-supplied strings to Erlang atoms with
`binary_to_atom/2`, on fields that arrive from external out-of-VM plugins
(event names, service names, method names, injected-service names). Erlang
atoms are never garbage-collected and the VM has a hard ceiling (about 1.05
million). An untrusted plugin can therefore send a stream of distinct strings,
mint unbounded atoms, and crash the entire node. For a kernel intended to
accept external plugin connections, this is a genuine denial-of-service hole.

The fix is to intern via `binary_to_existing_atom/2` with a binary-key
fallback, so unknown wire strings never create new atoms.

### 2. Bus fan-out uses flat globbing — confirmed, spec/impl drift

The message bus matches every published message against every subscriber by
splitting the topic string and running a glob, where the design specified a
prefix-tree index. This is a performance and scalability gap rather than a
correctness bug, and is likely acceptable at the current desktop-scale target,
but it is a real divergence between the design and the implementation.

## Verdict: net positive for the design

**The strong, positive side.** A fresh AI reconstructed an accurate model of
the kernel from source alone — the fiber process tree, the lock-free ETS
tables, the dispatch path that bypasses the central process, the reactive
dependency gating, and the reversible-effect teardown. That legibility is the
whole point of a tiny atom kernel, and it held up. On top of that, the agent
surfaced a real security bug and a real design drift, and produced runnable,
self-tested code rather than just prose.

**The honest caveats.** The proposals are code-grounded sketches, not proven
patches. Two of the five restate items already known in the design notes, so
they are rediscovery rather than novelty. Later rounds mostly re-enriched
earlier work — the fixed task loop plateaus after roughly one full pass. And
the rate-limiting shows that a subscription-tier account is a poor fit for
sustained autonomous runs; a pay-as-you-go API is the right tool for that,
while the interactive agent stays for interactive work.

The single most valuable outcome: an independent AI found a real
atom-exhaustion DoS in the kernel before it reached wide exposure.

## Follow-ups

Changes are scoped to **infrastructure, runtime, and the tool-call and
perception path**. Memory, content management, and agent-looping concerns are
deliberately out of scope here — they belong to a separate memory
architecture and are only noted on the roadmap.

- Apply the atom-safety fix to the kernel, with tests that prove malicious
  wire keys cannot mint atoms.
- Apply concurrent tool execution in the harness (the agent's "hands"), with
  tests preserving result order and side-effect classification.
- Evaluate the wire-backpressure and bus-trie proposals against the runtime
  budget and apply if they fit cleanly.
- Roadmap only (deferred to the memory architecture): in-harness context-window
  compaction and agent looping.

## Reproducibility

The full run — prompts, redacted per-round logs, the scheduler journal, and
every produced artifact — is archived out-of-band with a redaction pass and a
secret scan. Raw agent logs and any credential material never enter this
repository.
