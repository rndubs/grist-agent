# ADR-0001: Out-of-process tool mechanism

- **Status:** accepted (2026-09-06)
- **Date:** 2026-09-06
- **Milestone:** P0.3 (spike) / P0.4 (decision) — see `docs/IMPLEMENTATION_PLAN.md`

## Context

The harness has two extension tiers (D8). Middleware and first-party tools are
compiled Rust behind the `ext` API and are never authored by the agent at
runtime. Everything the agent *can* author at runtime — tools, skills,
workflows, profiles — runs out of process under the inner sandbox (D5), and
the tools among them need a hosting mechanism. Dev plan §15 listed three
candidates: process-based JSON-RPC, WASM Component Model plugins, and
in-process scripting. D5 rules out in-process scripting for anything from the
mutable layer, leaving two.

The decision has to be made before P1.7 (the `Session` launcher) and P2.1
(tool manifest and loader) are specified, because both are shaped by it. The
test case was the harness's primary domain tool (dev plan §7): a persistent
Python REPL as a `Session`-kind tool, which needs state across calls, numpy and
meshing libraries, the ability to run meshers and `sbatch`, and cancellation
per D15.

Spike write-up: `docs/spikes/extension-mechanism.md`. Both prototypes were
built and measured; the write-up records what could not be exercised (bwrap
was not available; GitHub was blocked).

## Options considered

1. **Process + JSON-RPC** — one long-lived process per `Session` tool, spawned
   under the derived bwrap policy; each call is a newline-delimited JSON-RPC
   2.0 request over stdio; cancel is SIGTERM then SIGKILL; a tool is a script
   plus a manifest.
2. **WASM Component Model** — the tool is a component with a fixed WIT
   interface, hosted by wasmtime inside a host process; calls are typed
   component calls; cancel is an epoch trap; a Python tool is built with
   componentize-py.
3. **In-process scripting** — excluded by D5 before the spike; listed for
   completeness.

## Decision

**Option 1: process + JSON-RPC over stdio**, for all agent-authorable tools.

The spike found both mechanisms adequate on state persistence, latency, and
cancellation: the process prototype answers a trivial call in ~74 µs (≈50 µs of
which is transport) and restarts in ~22 ms; the WASM prototype answers in
~55 µs and restarts in ~5 ms. Neither number is visible against a model turn.
The decision is made on authorability and on the domain's dependency story,
where the two diverge sharply:

- With option 1 the tool *is* the Python file the agent writes, run by the
  interpreter already present in the sandbox image, with dependencies from
  ordinary wheels (numpy 2.4.6 installed and used in the spike). It can run
  subprocesses (meshers, `sbatch` under a `Proc` grant) and threads.
- With option 2 every tool embeds a 19–23 MB CPython, needs a 43 MB build tool
  and a 4–7 s build step, only contains the stdlib modules imported at build
  time, cannot import numpy (no WASI wheels on PyPI; extension modules are
  native `.so`), has no `subprocess` at all (`wasi does not support
  processes`), no threads, and runs interpreted code 2–7x slower. Meshing and
  visualization libraries have no WASI builds.

Option 2's genuine advantages — a capability sandbox that needs no user
namespaces, microsecond calls, millisecond restarts — do not offset losing the
domain's libraries and process spawning. It is kept in reserve (see
consequences) rather than rejected outright.

This ADR also records the **D8 tier split** as the standing boundary:
compiled Rust middleware and first-party tools via `ext`; agent-authorable
extensions are out-of-process tools only, hosted by the mechanism above. The
agent never authors middleware.

## Consequences

**P1.7 — `Session` launcher.**
- One sandboxed process per session per `Session`-kind tool, spawned under the
  bwrap policy derived from the tool's capability atoms (D5, D6), with stdio
  piped, a scrubbed environment (D10), and `kill_on_drop`.
- Calls are JSON-RPC 2.0 requests, one JSON object per line. Tool-level
  failures (an exception in user code) are successful calls carrying a
  structured error; JSON-RPC error objects are for protocol failures only.
- Cancellation (D15): a per-call deadline; on expiry send SIGTERM, wait a
  short grace, then SIGKILL; log the cancellation; respawn on the next call.
  Because a process under `--unshare-pid` is PID 1 in its namespace and ignores
  an unhandled SIGTERM, the SIGKILL escalation is mandatory. Delivery under the
  real P0.1 stack must be verified when bwrap is available; the spike could not.
- Session state is lost on cancel or crash by design; the tool restarts clean.
- `Stateless`-kind tools use the same protocol with a process per call, so the
  launcher has one code path.

**P2.1 — manifest and loader.**
- A tool is a directory: a manifest naming the command to spawn, the JSON
  schema exposed to the model, the capability atoms it requires, and its
  Python (or other) dependencies. The loader materialises dependencies at
  install time (a `uv` venv per tool or per catalog), never per call.
- Authoring a tool at runtime is writing that directory; no compile step, no
  host restart. The catalog validates the manifest against the profile's
  grants (D7) before the tool is exposed.
- The wire protocol should be the stdio-MCP shape of JSON-RPC (`initialize`,
  `tools/list`, `tools/call`) or a strict subset, so that the same launcher
  serves agent-authored tools and the P2.2 MCP client. Whether the harness
  adopts MCP's schema verbatim or defines a subset with an adapter is
  ADR-0004's decision; this ADR only requires that a single launcher can host
  both.
- Results above the size cap spill to the artifact store (D12) instead of
  travelling inline; a 1 MB inline result costs ~7 ms and grows linearly.
  Binary outputs are written to the sandboxed workspace and returned as
  handles.

**What becomes harder.**
- The inner sandbox depends entirely on bwrap. If P0.1 finds that bwrap does
  not nest under the login node's uid map and no fallback exists, the WASM
  route is the only inner-sandbox option that needs no kernel features, and
  this ADR must be revisited (ADR-0002 will say which way P0.1 went).
- Every tool process carries a Python startup (~20 ms) and its own memory;
  sessions with many `Session` tools pay that per tool. Acceptable at the
  scale of a session.

**Held in reserve.** A second out-of-process kind, `Wasm`, for pure-compute,
dependency-free, untrusted tools, can be added later behind the same manifest
(`runtime = "wasm"`) without changing the kernel's `Tool` trait or the
launcher's contract. Nothing in this decision precludes it; nothing in P1 or
P2 should build it.
