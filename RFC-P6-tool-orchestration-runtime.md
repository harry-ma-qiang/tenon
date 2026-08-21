# RFC-P6: Tool Orchestration Runtime (control flow over tools)

Status: ROADMAP / idea capture. Not scheduled. Origin: the AI self-evolution
experiment's proposal 03 (parallel tool execution), promoted from a single
optimization to a phase-level capability.

## The idea

Today the harness executes one model step, then runs that step's tool calls
strictly one after another (`rs/harness/src/agent.rs`, the `for call in calls`
loop). Every branch, loop, or fan-out costs a full model round trip: the model
must see each result before it can decide the next call.

P6 moves simple control flow into the runtime, so the model can emit a small
*program over tools* that the runtime executes deterministically:

- `parallel` / `{ at the same time }` — run independent tool calls concurrently.
- `for` — map a tool over a list of inputs.
- `if` / `else` — branch on a prior result without a round trip.
- `batch` — group calls into one unit of work with one result set.

The model still decides *what* to do; the runtime handles the *plumbing* of
running it. Fewer round trips, lower latency and cost, and the agent's "hands"
become able to express a computation, not just a single grab.

## Why it needs strong isolation (the hard part)

Parallel and looped tool execution is only safe if concurrent calls cannot
corrupt each other or the host. This is the gating problem, and the reason
this is a phase, not a patch:

- **Effect ordering.** Results must be applied to the session in a
  deterministic order regardless of completion order, or the model's view of
  history becomes nondeterministic.
- **Side-effect classification.** Two mutating calls in one batch (two edits to
  one file, two shell commands) must not race. The tool contract needs a
  `parallel_safe` signal; `rs/harness/src/tools.rs` `Row` has none today.
- **Execution isolation.** The user's target: run each parallel/looped tool in
  its own isolated unit. Candidate backends, cheapest to strongest:
  - thread-level isolation (OS threads + per-call scratch, no shared mutable
    state),
  - a small language runtime per call (WASM via wasmtime, or a JS isolate),
  - a small VM per call (Firecracker / krun microVM, gVisor).
  Tenon already has `rs/sandbox` (oci / landlock / none / krun placeholder);
  P6 would extend it with a lighter-weight per-call isolation tier suited to
  fan-out, where one heavyweight OCI container per call is too expensive.

## Two design directions (pick during scheduling)

1. **A tiny tool-plan mini-language.** The model emits a small typed plan
   (DAG of calls with `parallel` / `for` / `if` nodes). The runtime validates
   and executes it. Deterministic, easy to sandbox, easy to audit. Closest to
   LLMCompiler's DAG.
2. **Code-as-action.** The model writes a small program (e.g. a restricted
   Python/JS/WASM dialect) that calls tools as functions; the runtime executes
   it in an isolate. Maximal expressivity (real loops, conditions, variables),
   but isolation and determinism are harder. Closest to CodeAct.

A hybrid is plausible: a restricted mini-language now (safe, auditable), with a
sandboxed code-as-action tier later behind strong isolation.

## Prior art (who else is doing this)

- **LLMCompiler** (Kim et al., ICML 2024): a Planner emits a DAG of function
  calls with dependencies, a Task Fetching Unit dispatches, an Executor runs
  them in parallel respecting dependencies. Reports up to 3.7x latency and 6.7x
  cost improvement over ReAct. This is the canonical parallel-function-calling
  design. https://arxiv.org/abs/2312.04511 ,
  https://github.com/SqueezeAILab/LLMCompiler
- **CodeAct / Executable Code Actions** (Wang et al., 2024): the action space
  *is* code run in a Python interpreter, so loops, conditions, variables, and
  result reuse come for free instead of one action per step.
  https://openreview.net/pdf/83841e7b4f455993deefb892159741a71a9c6482.pdf
- **Adoption**: the LLMCompiler pattern is a first-class LangGraph tutorial and
  appears in CrewAI, AutoGen, and OpenAI's Agents SDK. Native parallel tool
  calls exist in the Anthropic and OpenAI APIs, but without runtime-side
  control flow or per-call isolation - which is where Tenon can differentiate.
- **Isolation backends worth studying**: Firecracker microVMs, gVisor,
  wasmtime (WASM), V8/Deno isolates - the "smaller VM / smaller runtime" tier.

## Where Tenon can be different

The bus already makes every tool call policy-gatable (`tools/pre-execute`
waterfall) and env-scoped. P6 executes a *plan* of such calls under that same
single authorizer, with per-call isolation from `rs/sandbox`. That combination
- control flow + policy gating + real isolation, in one small kernel - is not
what the frameworks above offer; they orchestrate but do not sandbox each call.

## Non-goals for P6

- No memory, content management, or agent looping at the reasoning level -
  that is the Engram & Precortex (brain/memory) design, out of scope here.
- No change to the model's freedom to just call one tool; the plan is an
  option, not a requirement.

## Addendum: exec_flow - LispAST-in-JSON (proposed language layer)

Proposed by the cli-agent (agy) during batch-1. A concrete realization of
design direction 1, and the current front-runner for the language layer.

The shape: a single tool `exec_flow(flow)` whose `flow` argument is a JSON
S-expression AST - "LispAST in JSON", homoiconic (code = data = JSON):

- Leaf: `[tool_name, args_obj]` evaluates to `tools.dispatch(tool_name, args)`.
- Combinators: `["parallel", child, ...]` (Tokio JoinSet), `["if", cond, then,
  else]`, `["for", ...]`, evaluated recursively in `rs/harness` over the
  existing ToolRegistry. The whole tree returns as one tool-result string.
- Smart fallback: when the model returns an ordinary array of tool calls that
  are all read-only, the scheduler transparently runs them in parallel and
  reassembles results in original order - no prompt or client change.

Why this is the right shape:
- LLMs emit JSON reliably; a homoiconic JSON AST is the most natural action
  space for them, with no bespoke grammar or parser.
- Code-as-data makes the program trivially validatable, sandboxable, and
  auditable - directly serving the determinism/isolation/audit goals. It is
  strictly easier to sandbox and make deterministic than code-as-action
  (arbitrary Python), while keeping if/for/parallel expressivity.
- Leaves reuse the existing ToolRegistry and the `tools/pre-execute` policy
  waterfall; only the combinators are new.

Honest caveats (must not be logged as settled fact):
- Performance claims (e.g. 50% token / 5-10x speed) are unverified; the
  measured reference is LLMCompiler's 3.7x latency / 6.7x cost, and gains are
  workload-dependent (only steps with independent I/O benefit).
- The AST solves expressivity + determinism + audit, NOT isolation. Parallel
  MUTATING calls still need the thread/WASM/microVM isolation tier underneath.
  Language layer is not the isolation layer.
- A model-controlled AST is an attack surface: enforce recursion-depth and
  size limits (the same untrusted-input DoS class as the atom-table bug), run
  every leaf under the policy gate and env-scope, and add a data-flow binding
  (`let` / `$ref`) so a node can consume a prior node's result.
- The smart fallback depends on the `parallel_safe` tool classification the
  harness does not have yet.

Prior art for a computable JSON DSL: JSONLogic (rules/logic as JSON),
s-expression homoiconicity (Lisp / MAL), plus LLMCompiler (DAG) and CodeAct
(code as action) already cited above.

## First concrete step when scheduled

1. Add `parallel_safe: bool` (default false) to the tool `Row` and thread it
   through registration (bus/MCP/built-in). Zero behavior change until a tool
   opts in.
2. In `steps`, run an all-`parallel_safe` batch concurrently with
   order-preserving effect application; everything else stays serial.
3. Only then add `for` / `if` and the per-call isolation tier.
