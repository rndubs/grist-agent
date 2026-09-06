# Agent Harness Development Plan

*A minimal, embeddable, self-improvable agent kernel for simulation and engineering workflows*

v0.2 — September 2026

*v0.2 folds in design decisions D1–D20 (`docs/design-decisions.md`), ADR-0001 and ADR-0003 (`docs/adr/`), and the Phase 0 spike outcomes (`docs/spikes/`). `docs/specs/` — `kernel-interface.md`, `event-schema.md`, `profile-schema.md` — is now the normative surface for the kernel, events, and profiles; where this plan and a spec differ, the spec wins and the plan is the bug.*

---

## 0. One-paragraph summary

We are building a small Rust agent kernel that can be embedded in native applications, run as a headless daemon on a laptop or inside a Podman container near an HPC cluster, and be driven by a separate UI. Everything that makes the agent *specific* — prompts, tools, skills, sub-agents, memory, control flow, sandbox policy — lives outside the kernel as versioned data so that teams can specialize it per repo and per model, and so that an outer "evolve" loop (human- or agent-driven) can change it safely and measurably. The system is built around one append-only event log that doubles as checkpoint stream, replay cassette, and provenance source, because the agent is both the orchestrator of simulation pipelines and a participant that must be traced like any other data-producing process.

The plan below explains *what* each part is and *why* it exists, then lays out phases, risks, and open questions.

---

## 1. Goals and non-goals

### Goals

1. **One core, many hosts.** The same kernel runs embedded in a desktop app, as a local daemon, and remotely in containers, without forking.
2. **Minimal by default, extensible by construction.** Ship the smallest useful agent; every additional feature — including MCP and sub-agents — is built on the same extension API third parties use.
3. **Specializable.** Teams build agents for their repos; we build agents for specific models (including fine-tunes); both compose without touching the kernel.
4. **Self-improvable, with guardrails.** The harness is a searchable object. The agent can propose changes to it; the system can measure whether they help; nothing is promoted on faith.
5. **Simulation-native.** Long-running jobs, large artifacts, provenance, and end-to-end mesh → solve → post-process pipelines are first-class, not bolted on.
6. **Sandboxed by default.** No permission popups; least privilege enforced mechanically.

### Non-goals (for now)

- Running the agent itself in a browser via WASM. The UI can be a web page; the agent is a daemon it talks to.
- A general workflow engine. We support graphs where a task is genuinely a graph; we do not reinvent LangGraph.
- Supporting every model provider on day one. We start with OpenAI-compatible endpoints (vLLM, LiteLLM) and add native provider clients later.

---

## 2. What we learned from prior art, and what we're taking

| Source | What it got right | What we take |
|---|---|---|
| **pi** (Zechner / Earendil) | Four tools, sub-1k-token system prompt, no built-in sub-agents/MCP/plan mode; one runtime exposed as interactive, JSON, RPC, and SDK; explicit, inspectable context; cross-provider session serialization | Tiny frozen kernel; first-party features as extensions; multiple run modes over one runtime; session format as a foundational contract |
| **dcode** (LangChain Deep Agents) | Model profiles that package prompt/tool-description/default adjustments per model; agent configs; sub-agents as markdown-with-frontmatter; model resolution order | The two-axis profile system (model × agent); file-based sub-agent definitions |
| **LangGraph / DeepAgents SDK** | Durable checkpointed state, interrupts, middleware hooks, conditional routing | Checkpoint-per-turn, suspend/resume, `Middleware` trait, workflows-as-data — *not* the graph object model |
| **Amp Orbs** | Executor placement (local / cloud / named runner), agent-to-agent messaging, self-scheduling, event-driven wake, multiplayer; distinct trust models for inbound triggers | Placement as an orchestrator concern; one protocol over any transport; trust tier per trigger source |
| **Meta-Harness** (Lee et al. 2026) | Outer loop gives the proposer raw filesystem access to all prior code, traces, and scores; minimal fixed scaffold | Full-fidelity trace archive that a meta-agent can grep; keep the outer loop dumb |
| **HarnessDev** (Wu et al. 2026), Harness-evolution critiques (Jul 2026) | Evolved harnesses overfit dev sets, regress silently, depend on runtime model, and often lose to parallel sampling at equal cost | Hidden held-out evals; parallel-sampling baseline; per-model profiles; promotion gates |
| **ALMA** (Xiong, Hu, Clune 2026) | Memory designs as executable code searched by a meta-agent with an archive | Memory as a pluggable module with store/retrieve/update/compress/forget; ALMA loop as one instance of our evolve loop |
| **Agent Client Protocol** (Zed) | JSON-RPC schema for editor ↔ agent | Candidate wire format for UI ↔ daemon, gets editors as free clients |

---

## 3. Architecture

```
┌──────────────┐   protocol (JSON-RPC / ACP over stdio + unix socket; websocket backlogged, D11)
│  UI clients  │◄──────────────────────────────────────────────────────┐
│ Tauri app,   │                                                       │
│ web, editor  │                                                       │
└──────────────┘                                                       │
                                                                       │
┌───────────────────────────── orchestrator ────────────────────────────┴───┐
│ placement (local | podman | slurm-runner) · wakers · trust tiers · fleet │
│                                                                          │
│   ┌────────── outer sandbox (bwrap, owned by launcher, invisible) ──────┐ │
│   │  ┌──────────── kernel process ─────────────┐                       │ │
│   │  │ loop · State · Tool · Middleware chain   │ ◄─ providers (vLLM,  │ │
│   │  │ event log → checkpoints                  │     LiteLLM)          │ │
│   │  └──────┬───────────────┬──────────────────┘                       │ │
│   │         │ ext            │ inner sandbox per tool call (bwrap)      │ │
│   │   MCP · sub-agents · skills · memory · workflows · python REPL      │ │
│   └──────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────────┘
          │ event log                                    ▲
          ▼                                              │ proposes changes to
┌────────────────┐    ┌───────────────┐    ┌─────────────┴────┐
│ artifact store │    │ provenance DB │    │ evolve loop      │
│ (CAS by hash)  │    │ (projected)   │    │ (meta-agent+eval)│
└────────────────┘    └───────────────┘    └──────────────────┘
```

**v0.2 — process model (D4, D14).** The runtime is tokio; `Tool`, `Provider`, and `Middleware` are async traits. One kernel process serves one session over stdio. The daemon (P1.9) is a *supervisor*: it spawns one kernel process per session and proxies the unix socket to it, and it is the seed of the Phase 3 orchestrator. The kernel is deployed only on Linux; clients run on Windows, macOS, and Linux. The sandbox backend is an explicit trait: `Bwrap` in production, and a `None` backend that compiles only in development builds and is logged in every run.

**v0.2 — extension tiers (D8, ADR-0001).** Middleware and first-party tools are compiled Rust behind the `ext` API. Everything the agent can author at runtime — tools, skills, workflows, profiles — is an out-of-process tool hosted as a long-lived process speaking newline-delimited JSON-RPC 2.0 over stdio under the inner sandbox (ADR-0001). The agent never authors middleware. WASM is held in reserve as a second out-of-process kind, not built in P1 or P2.

### 3.1 Crate layout

| Crate | Responsibility | Mutable by evolve loop? | Filled in at |
|---|---|---|---|
| `kernel` | Agent loop, typed `State`, `Tool`/`Middleware`/`Provider`/`Host` traits, `ArtifactStore` and `Memory` traits (no-op impls in P1, D12), result spill, `SandboxPolicy` + `derive_policy` (pure, D6), hashing, event log, checkpoints, suspend/resume, record/replay. Depends on nothing in-repo. | **No** (soft freeze at P1 exit, hard at P2 exit) | P1.1–P1.4 |
| `providers` | Model clients. v0: one OpenAI-compatible client with per-endpoint quirk flags (covers vLLM, LiteLLM, and the llama.cpp CI stand-in; ADR-0003) | No | P1.5 |
| `host` | Implementations of the kernel's `Host` trait — filesystem, process spawn, network, secrets as handles (D10), `ask_user` (D17): `native`, `remote-client` (stub until P3) | No | P1.6, P3.3 |
| `ext` | Extension API + first-party extensions: out-of-process tool manifest and loader (ADR-0001), MCP client, sub-agent spawn, skills, memory modules, workflow runner, capability gate | Extensions yes; API no | P2.x |
| `profiles` | Loading/merging/validating model profiles, agent profiles, project overrides; bundles expansion; catalog | Content yes; loader no | P1.8 |
| `sandbox` | Enforcement of the inner policy: `SandboxBackend` impls (`Bwrap`, dev-only `None`), bwrap argument generation, `Stateless` and `Session` launchers; hosts the six base tools in `src/tools/` until P2.1 | Policy yes; enforcement no | P1.7 |
| `orchestrator` | P1.9 supervisor daemon (seed); placement, wakers, fleet, agent-to-agent messaging, trust tiers | No | P1.9, P3.3–P3.7 |
| `provenance` | Event-log projector → relational PROV schema; artifact store | Schema no | P2.5, P3.1–P3.2 |
| `evolve` | Outer loop: proposer harness, eval runner, promotion gates | Yes (it's a profile too) | P4 |

The `ui` client (Tauri app / web build) is not a workspace crate; it lands in P1.9 under its own directory once ADR-0004 picks the wire format. The authoritative crate list and dependency DAG are in `crates/README.md`.

**v0.2 — where things actually live.** Policy *derivation* (`SandboxPolicy`, `derive_policy`) is in `kernel` so the kernel can derive every tool's policy at construction without depending on `sandbox`; `sandbox` only turns a policy into bwrap arguments and launches processes (`docs/specs/kernel-interface.md` §3.12). The six base tools (`read`, `write`, `edit`, `bash`, `run_script`, Python REPL) live in `crates/sandbox/src/tools/` until the `ext` API lands in P2.1. Mutable profile content — `catalog.toml`, `bundles.toml`, `models/`, `agents/` — lives in a top-level `profiles/` directory that `crates/profiles` loads.

**Why this split:** the "frozen kernel / mutable everything else" line is the single most important boundary in the system. It is what makes self-modification safe (the agent cannot edit the loop or the sandbox), makes provenance honest (the harness records, the agent annotates), and defines the search space for evolution (all the mutable files).

---

## 4. The kernel

*v0.2: `docs/specs/kernel-interface.md` (signatures, state machines, loop, semantics) and `docs/specs/event-schema.md` (envelope, event kinds, hashes, redaction, replay) are normative. This section is the rationale.*

### 4.1 The loop

```
loop {
    middleware.before_model(&mut state)
    resp = provider.complete(state.context())
    middleware.after_model(&mut state, resp)
    for call in resp.tool_calls {
        middleware.before_tool(&mut state, call)
        result = tools.invoke(call)          // Value | Task{..}
        middleware.after_tool(&mut state, result)
    }
    checkpoint(&state)
    if state.only_pending_tasks() { suspend() }   // resumed by orchestrator waker
    if state.done() { break }
}
```

It is a plain loop, not a graph. Every harness experiment we know of (compaction, retrieval, verification gates, tool-format parsing) fits in the four hooks.

**v0.2 — made precise (D1, D2, D15; kernel-interface §6–§7).** Suspension rule: a turn ends with no tool calls and pending tasks exist. Task completion arrives as a `TaskUpdate` event from a waker; the kernel appends a synthetic tool result carrying the outcome, resuming the session if suspended or injecting it at the next turn boundary. A cancellation token is checked between hooks; the sandbox launcher sends SIGTERM to a running tool and the cancellation is logged. Provider rate-limit and server errors are retried with backoff up to a fixed count, then the turn fails and the session enters `failed` with its checkpoint intact. On start with an existing log the kernel restores the last checkpoint and discards events after it. The loop can only invoke tools registered at construction.

### 4.2 Core types

- **`State`** — typed Rust struct (serde): messages, `pending_tasks`, `session_status` (D2), active profile hashes, memory pointer, notebook path, sandbox policy hash, sandbox backend name (D14). **Versioned from day one** (`schema_version` field, with a migration registry) because the evolve loop will fork old checkpoints.
- **Session states (D2)** — `created`, `running`, `idle` (waiting for user), `suspended` (waiting for a waker), `done` (explicit end only), `failed` (last checkpoint kept, resumable). A user message in `idle` or `suspended` moves to `running`; a message while `running` is queued until the turn boundary.
- **`Tool`** — `name`, `description`, `schema`, `kind: Stateless | Session` (D5), `capabilities: Vec<Capability>`, async `invoke() -> Result<ToolResult>`. `ToolResult` is `Value | Task { id, status, eta, check_hint }`; task states are `pending`, `running`, `succeeded`, `failed`, `cancelled` (D1). `check_hint` is for the polling sidecar and is never shown to the model. Results over a size cap are spilled through the kernel's `ArtifactStore` and replaced by `{handle, head, tail}`; the spill lives in the kernel so a profile cannot disable it (D12).
- **`ArtifactStore` and `Memory`** — traits defined in `kernel` from P1 with no-op implementations (D12); real implementations arrive in P2.
- **Content blocks** — text, `Thinking { text, signature }` (survives serialization and replay across providers, ADR-0003), tool use, tool result, and `Image { artifact_handle, mime }` — images are artifact handles, and the provider encodes bytes at request time (D15).
- **`Middleware`** — async `before_model`, `after_model`, `before_tool`, `after_tool`, `on_compact`, `on_resume`. Model and agent profiles contribute named entries with a priority; the chain is a stable sort (D7) and the *resolved* chain is logged into every run.
- **`Event`** — every message, model request/response, tool call, tool result, task start/update, checkpoint, suspend, resume, cancellation, turn/session failure, recovery, profile load, `ask_user`/answer, plus reserved kinds for compaction, spawn, and harness edit. Append-only JSONL, one file per session, never rewritten. Every line is an envelope `{seq, ts, session_id, kind, payload}` (D3). Tool call events carry the argument hash; tool result events the result hash and artifact handles; model call events the request hash, response hash, token usage, model id, and active profile hashes (D13). This log is simultaneously the session file, the checkpoint stream, the replay cassette, and the provenance feed.
- **Hashing (D3)** — BLAKE3 over RFC 8785 canonical JSON; hashes cover the `payload` only, never envelope fields; hash strings carry an algorithm prefix (`b3:<hex>`). The state hash covers `State` minus an enumerated list of volatile fields (event-schema §3.5). The log writer redacts known secret values and common token patterns before writing (D10).

### 4.3 Why async tasks live in the kernel

An HPC job is not a slow tool call; it is a tool call that returns a handle. If the kernel understands `Task`, the agent never polls in context, the process can be released for hours, and resumption is an ordinary checkpoint restore triggered by an external waker (Slurm epilog, file watcher, webhook, polling sidecar). Putting this in an extension would force every long-running tool to reinvent it.

v0.2 (D1): a tool returning a `Task` appends a "task started" tool result and the model may keep working; tasks live in `State.pending_tasks`; wakers deliver `TaskUpdate` events. Phase 1 ships an in-kernel process-exit waker for the `run_script` tool; the external wakers are Phase 3.

### 4.4 Record/replay

Model responses and tool results are recorded keyed by `(checkpoint_hash, request_hash)`. Middleware and profile changes can then be tested against recorded runs in milliseconds with no API spend and no nondeterminism. This is what makes harness iteration feel like normal software engineering, for humans and for the evolve loop.

v0.2 (D16): the cassette *is* the log. "Deterministic replay" means the replayed event payloads, after stripping envelope fields (and the enumerated volatile fields, event-schema §5.3), are byte-identical to the recorded ones, asserted by a `diff-logs` command. A cache miss during replay is an error, never a live call.

---

## 5. Providers: vLLM and LiteLLM first

Both expose OpenAI-compatible `/v1/chat/completions` with SSE streaming, so v0 is **one client with quirk flags**, not two providers:

| Concern | vLLM | LiteLLM proxy | Handling |
|---|---|---|---|
| Model naming | bare served name | `provider/model` routed by proxy | Model profile carries the full string; kernel treats it as opaque |
| Tool calling | requires server started with auto-tool-choice + a tool-call parser; behavior varies per model | passes through to upstream; varies per upstream | Model profile declares `tool_format: native | parsed(<syntax>)`; a parser middleware normalizes non-native formats |
| Reasoning/thinking | `reasoning_content` field (model-dependent) | upstream-dependent, sometimes remapped | Provider maps whatever field exists into a `Thinking` content block; serialization preserves it across providers |
| Structured output | JSON schema / guided decoding flags | upstream-dependent | Expose as a provider capability flag; middleware falls back to prompt+parse if absent |
| Auth / base URL | usually none / local | API key + proxy URL | `Auth::None` or `Auth::Bearer(SecretHandle)`; the handle is resolved through the Host at request time (D10); the provider never reads the environment |
| Context length | server-configured | per-model in proxy config | Model profile declares it; compaction middleware reads it |

**Why this order:** fine-tuned and local models will be served by vLLM; LiteLLM gives us every hosted provider through one door and centralizes keys and spend. A native Anthropic/OpenAI client comes later (backlog, after P2) as an additional `Provider` implementation, mainly for prompt caching and provider-specific features.

**v0.2 — decided (ADR-0003, from the P0.2 spike).** The P0.2 spike confirmed that every observed difference between vLLM, LiteLLM, and the llama.cpp CI stand-in (D18) is flag-sized, so P1.5 ships a single `OpenAiCompatProvider { endpoint, quirks }` behind the `Provider` trait. `Quirks` lives in the model profile (D7) and is content-hashed with it: `reasoning_field` (`none | reasoning_content | reasoning | provider_specific_fields | inline_think`), `tool_format` (`native | parsed(<syntax>)`), `supports_structured_output` (`yes | no | unknown`, filled by a probe, not detected at runtime), `supports_stream_usage`, `auth`, `strict_tool_schema`, `sends_finish_reason_tool_calls`, `streams_tool_call_fragments`. The verified quirk table is `docs/spikes/providers.md` §4; adding an endpoint is a config row plus one probe run. Rules that fell out of the spike: tool-call accumulation is keyed by `index` and never relies on `finish_reason`; one `Thinking` block per response, assembled from whichever field the flag names; always request `stream_options.include_usage` and take usage from any chunk that carries it; non-native tool syntaxes (`parsed(<syntax>)`) are normalised by a *middleware* in the D7 fixed early slot, not inside the client — the provider exposes raw text plus native tool calls. Still open (fills rows, changes nothing): the exact `parsed(<syntax>)` per vLLM model and reasoning behaviour for hosted upstreams through LiteLLM, both awaiting a human run.

---

## 6. Profiles and the catalog

Two orthogonal layers over the kernel, both as TOML files (D7), both versioned. *v0.2: `docs/specs/profile-schema.md` is normative for the schemas, merge rules, resolution algorithm, validator cases, and capability grammar; the content ships in the top-level `profiles/` directory.*

**Model profile** (keyed by `endpoint:model`): system prompt variant, tool-description phrasings, tool-call format, thinking/temperature defaults, context length, compaction thresholds, provider `quirks` (ADR-0003), and — for fine-tunes — the profile hash the model was trained against.

**Agent profile** (per team / repo / role): capability grants, MCP servers (registered lazily), skills, `AGENTS.md`, sub-agent definitions, middleware entries, memory module, sandbox policy, eval set pointer, and a per-turn context budget (placeholder 40k tokens, to be tuned; D16).

**Capabilities are typed atoms (D6):** `Fs{path, mode}`, `Net{allowlist}`, `Proc{spawn}`, `Tool{name}`, `Spawn{catalog_name}`, `Secret{name}`. Narrower-than is defined per atom (path prefix, `ro` below `rw`, allowlist subset, exact name). Domain bundles (`meshing`, `solver`, `post`) are named atom sets in `profiles/bundles.toml`, expanded by the `profiles` crate at resolution time; the kernel and sandbox see only atoms. HPC in-house tools are granted as `Fs{<install path>, ro}` plus `Proc{sbatch}`.

Resolution: `kernel defaults + model profile + agent profile + project overrides`. Merge rules (D7): scalars override, tables deep-merge, lists replace. Middleware: model and agent profiles contribute named entries with a priority; the resolved chain is a stable sort, and the model profile's tool-call parser has a fixed early slot. The system prompt is assembled as separate blocks in fixed order: model prompt variant, agent role prompt, `AGENTS.md`, active skills, notebook (on resume). Profiles override *values*, never *structure*; a profile cannot replace the loop or widen the sandbox — the validator rejects unknown keys, kernel-only keys, and capabilities exceeding grants. Every loaded profile is content-hashed; the hashes are recorded in `State` and in a `ProfileLoad` event.

**Task agent** = (model profile, agent profile) pair registered in the catalog. Fine-tuned specialists and generic sub-agents are the same kind of thing; a parent simply spawns by catalog name.

**Sub-agent scoping** is enforced at three levels: (1) the kernel's tool registry only invokes tools registered at construction — the model cannot call what isn't registered; (2) the inner sandbox policy is derived from the same list, so bash cannot route around it; (3) `spawn` narrows monotonically — a child's capabilities are a subset of its parent's. A child needing more sends a request up rather than acquiring a tool.

**Why:** this is what lets a team own its agent as a git repo while the kernel stays upstream; it is the search space for evolution; and per-model profiles are the direct answer to the finding that harness gains are model-specific.

---

## 7. Context discipline (the simulation-domain defaults)

We will drown the context window unless these are defaults, not options:

1. **Persistent Python REPL as the primary domain tool.** Meshing, FEA setup, and post-processing are code with state that must persist across calls (a loaded mesh, a results dataset). One `python` tool backed by a session-scoped kernel inside the inner sandbox replaces dozens of MCP tools; the model scripts against libraries instead of orchestrating tool calls.
2. **Lazy tool exposure.** MCP servers register but their schemas stay out of the prompt. A `find_tools(query)` tool surfaces relevant ones for the next turn; skills reference tools by name.
3. **Artifacts, not data.** Content-addressed store; tool results over a cap spill automatically; solver logs get a structured-extraction pass (errors, convergence, timings); visualization returns PNGs.
4. **Externalized working state.** A lab-notebook file the harness re-injects on resume; compaction summarizes *toward* the notebook, not into a lossy paragraph.
5. **Scoped sub-agents** partition context by role.

---

## 8. Sandboxing

Two sandboxes with different jobs:

- **Outer (bwrap around the whole kernel process).** The trust boundary. Owned by the launcher/orchestrator, invisible to the harness, sits *outside* the mutable layer so self-modification cannot weaken it. Tier chosen by trigger source (interactive > scheduled > inbound webhook).
- **Inner (bwrap per tool process).** Least privilege as the permission model: repo RW, everything else RO, network off unless the capability is granted, tmpfs scratch, timeout, scrubbed environment. Derived mechanically (`derive_policy`, in `kernel`, no escape hatch) from the tool's capability atoms and the profile's grants (D6). Sub-agents get narrower policies than parents.

The inner policy is declarative and may be evolved; the outer never is.

**v0.2 — sandbox shape (D5, D8, D10, D14, ADR-0001).** A `Tool` has a `kind`: `Stateless` (fresh bwrap per call; bash state does not persist) or `Session` (one bwrap process per session under the derived policy; calls are RPC into it — the Python REPL, MCP servers, and every agent-authored tool). First-party compiled tools (`read`, `write`, `edit`) enforce the same policy via path checks in the Host layer, in process. Everything from the mutable layer — bash commands, Python code, agent-authored extensions — runs out of process under bwrap. Per ADR-0001 the out-of-process mechanism is a long-lived process per `Session` tool speaking newline-delimited JSON-RPC 2.0 over stdio, spawned with stdio piped, a scrubbed environment that never inherits API keys (D10), and `kill_on_drop`; `Stateless` tools use the same protocol with a process per call, so the launcher has one code path. Cancellation (D15) is a per-call deadline, then SIGTERM → short grace → SIGKILL (mandatory, since a process under `--unshare-pid` ignores an unhandled SIGTERM); session state is lost on cancel or crash by design. Tool-level failures are successful calls carrying a structured error. The backend is an explicit `SandboxBackend` trait with `Bwrap` and a dev-build-only `None` implementation that is logged in every run (D14); the kernel runs only on Linux.

**Deployment target (D18).** The kernel runs in rootless Podman on the HPC login node under the site's uid map (`podman run --uidmap 0:0:2000 --uidmap 65534:2000:2`; build with `--userns-uid-map=0:0:1 --userns-uid-map=1:1:1999 --userns-uid-map=65534:2000:2`). CI has a CPU stand-in stack (llama.cpp behind a real LiteLLM proxy, a fake `sbatch` that calls the epilog hook, and a mock solver); the fake-Slurm and mock-solver tests run on every PR, the model-serving part is opt-in (PR label `ci:standin` or a manual run).

**Known hazard — still open (P0.1, ADR-0002 pending).** bwrap nesting inside rootless Podman requires unprivileged user namespaces and often seccomp/userns adjustments, and the site map has only 2002 uids and no `subuid` range. The P0.1 scripts (`spikes/sandbox-nesting/`) test Podman → outer bwrap → inner bwrap under exactly that map and are ready, but the run needs the login node and has not happened; ADR-0002 waits on it. Fallback order: bwrap directly on the login node (container becomes a packaging concern), then namespace-per-container or gVisor; if no inner bwrap exists at all, the WASM route held in reserve by ADR-0001 is the only inner-sandbox option needing no kernel features, and ADR-0001 must be revisited. P1.7 has landed and is tested with the `None` backend; its five `bwrap` tests skip until run on a host with `bwrap`.

---

## 9. Workflows and middleware (what we borrow from LangGraph)

*The loop is code, the workflow is data, middleware is the seam.*

- **Middleware** is the primary extension shape (§4.2).
- **Workflows** are an `ext` module: nodes are {task agent, tool, pure function, sub-workflow}; edges are static or routing functions; declared in TOML with a small Rust builder for cases needing real code. Used only where a task is genuinely a graph — mesh → setup → solve → post → viz pipelines, nightly triage fan-outs, and the evolve loop itself.
- **Rule to prevent DSL creep:** if a node needs loops-with-state, it is an agent profile, not a workflow feature.

**Why:** workflows and middleware stacks as data are diffable, versionable, and editable by the meta-agent; checkpoint forking ("replay run 47 from turn 12 with the new compaction middleware") makes harness experiments cheap and well-controlled, and doubles as time-travel debugging for humans.

---

## 10. Provenance and data management

One model for simulation products *and* agent activity, based on W3C PROV:

| PROV noun | Simulation side | Agent side |
|---|---|---|
| **Entity** | meshes, input decks, result files, logs | prompts, checkpoints, memory snapshots, skill files, harness/profile versions, sandbox policies |
| **Activity** | meshing job, solver run, post-processing | model call, tool call, compaction, spawn, harness edit |
| **Agent** | human, external system | `(model_id, model_profile_hash, agent_profile_hash, kernel_version)` |

Edges: `used`, `generated`, `wasDerivedFrom`, `wasAttributedTo`, `wasInformedBy`. Everything content-addressed.

Design rules:

1. **The harness records; the agent annotates.** Pedigree rows are emitted mechanically at tool boundaries and by wrappers around external systems (Slurm, solvers). The agent may attach rationale and hypotheses as entities attributed to it; it never edits the graph.
2. **The agent is a versioned configuration, not a blob.** When the evolve loop changes a profile, the result is a *new* agent with a `wasDerivedFrom` edge. Self-modification is an ordinary provenance event.
3. **Event log is truth; tables are projections.** A projector folds JSONL into SQLite (local) or Postgres (fleet). "What happened" is immutable; "what is current" is a derived view; corrections never rewrite history.
4. **Blobs out of band.** The relational layer holds metadata and edges; bytes live in the CAS by hash.

Payoff: "why is this mesh 2 mm?" resolves to a checkpoint whose reasoning trace, skill, and memory are one hop away — something conventional SPDM cannot answer. Per-profile tool-usage statistics, sub-agent request patterns, and eval outcomes all fall out of the same tables and feed the evolve loop.

---

## 11. Orchestration

- **An agent is a kernel process speaking the protocol over a transport.** Local GUI, remote daemon, and sub-agents are indistinguishable to the UI. v0.2 (D4): one kernel process serves one session over stdio; the P1.9 daemon is a supervisor that spawns a kernel process per session and proxies the unix socket to it, and grows into the Phase 3 orchestrator.
- **Transport and auth (D11).** Unix socket with file permissions plus a peer-credential check. Remote access is an SSH-forwarded unix socket to the HPC login node. Websocket auth and OpenShift access patterns are backlogged pending the security team.
- **`ask_user` (D17).** The Host's UI-prompt capability is a tool for asking the user a question; the answer is logged as a user message and round-trips through the protocol. Permission is never asked; the sandbox policy already answered it.
- **Placement** is an orchestrator decision: `local`, `podman`, or a named `runner` (e.g., a cluster login node with the filesystem mounted). Run near the data. The first target is rootless Podman on the HPC login node (D18); the opt-in CI stand-in (fake `sbatch` + epilog, mock solver, llama.cpp behind LiteLLM) stands in for it in CI.
- **Wakers** resume suspended agents: Slurm epilog, file watcher, cron/self-schedule, inbound webhook. Each source maps to an outer-sandbox trust tier. Phase 1 ships only the in-kernel process-exit waker (D1).
- **Agent-to-agent messaging** uses the same protocol; external agentic systems are wrapped as tools that return `Task` handles, so a slow collaborator looks exactly like a slow solver.
- **UI**: ADR-0004 (accepted) adopts ACP v2 as the wire format, so the first client is an off-the-shelf ACP editor (Zed) reached through the `grist-connect` stdio↔socket forwarder. The Tauri app — whose web frontend is intended to also serve as the remote daemon's browser UI once websocket transport is unbacklogged (D11) — moves to P3, where that transport and a purpose-built fleet view are actually in scope.

---

## 12. The evolve loop

The research consensus is sobering: evolved harnesses overfit dev sets, regress silently, are model-specific, and frequently lose to simply sampling more at equal token cost. The one pattern that reliably worked was a proposer with access to full raw traces finding a *concrete* failure mode (e.g., runs reporting success that did not pass) and shipping a *verified* fix. We design for exactly that:

1. **Search space** = the mutable layer: prompts, skills, tool descriptions, middleware chains, workflows, memory modules, inner sandbox policy, tool allowlists. Never the kernel.
2. **Archive** = the full event log + artifact store + provenance DB, exposed to the proposer as a filesystem it can grep. The outer loop stays deliberately dumb.
3. **Evaluation** = each profile has a visible dev set and a *hidden* held-out set; candidates are also compared against a parallel-sampling baseline at equal token cost; efficiency (tokens) is reported alongside success.
4. **Promotion gate** = held-out gain, no regression on any other profile's eval, baseline beaten, diff reviewed (human or reviewer-agent) and provenance-linked. Sandbox-policy changes get a static check.
5. **Cheap experiments** = checkpoint forking and record/replay before any live run; Podman-per-candidate for live runs.
6. **Simulation-domain evals judge process, not exact numbers** (correct setup, convergence, sane post-processing, tolerances) because solver nondeterminism would otherwise let the meta-agent optimize noise.
7. **Memory evolution** (ALMA-style) and **tool-allowlist pruning** are the first targets: low risk, high signal, directly measurable.
8. **Training-data closure:** successful, profile-labelled traces are exportable as fine-tuning data, so harness evolution and weight training share one source.

---

## 13. Phased roadmap

*v0.2: milestone-level status lives in `docs/IMPLEMENTATION_PLAN.md`; the status sentences below are a snapshot as of this revision.*

**Phase 0 — Spikes (de-risk before design freeze)** — *in progress*
- Podman → outer bwrap → inner bwrap nesting works in the target cluster environment under the site uid map (D18). *Status: scripts and write-up template ready in `spikes/sandbox-nesting/`; the run needs the login node; ADR-0002 pending.*
- OpenAI-compatible client against vLLM (tool calling + reasoning field) and LiteLLM (routing + auth), plus the llama.cpp stand-in. *Status: client built; verified against emulated shapes, a real LiteLLM proxy, and the CI stand-in; real vLLM and hosted-upstream runs need a human. ADR-0003 accepted.*
- Extension mechanism decision: process-based JSON-RPC vs. WASM Component Model (in-process scripting excluded by D5). *Status: both prototyped with the Python REPL as the test case; **decided** — process JSON-RPC over stdio, ADR-0001 accepted; WASM held in reserve.*
- CI stand-in stack (D18): fake `sbatch`/epilog and mock solver on every PR; llama.cpp + LiteLLM opt-in. *Status: landed; the opt-in job has gone green.*
- Exit criteria: all three spikes green (nesting still pending); extension mechanism chosen and written up (done).

**Phase 1 — Kernel + local daemon** — *P1.0–P1.8 landed; P1.9 pending*
- Interface specs (`docs/specs/`, D20) approved before any core code.
- `kernel`, `providers`, `host::native`, event log, checkpoints, suspend/resume, record/replay, `diff-logs`.
- Six base tools — `read`, `write`, `edit` (in-process Host path checks), `bash` (Stateless), `run_script` returning a `Task` with the in-kernel process-exit waker (D1), Python REPL as a `Session` tool — all under the inner policy.
- Profiles (model + agent, TOML), bundles, validator, catalog, one default agent.
- Minimal protocol server (stdio kernel + unix-socket supervisor, D4/D11); CLI-less: ACP v2 with Zed as the first client (ADR-0004, accepted). *Not started.*
- Exit: a coding session and a "run a script, suspend, resume on completion" session both replay deterministically from the log (`diff-logs`, D16); a session reaches `failed` on provider exhaustion and resumes from its checkpoint; `schema_version` migrations exercised; `kernel` soft-frozen (D12). *Status: the first three and the migration criterion are met; the soft freeze is a human decision at phase exit, after P1.9. The bwrap-backed tests of P1.7 await a host with `bwrap`.*

**Phase 2 — Extensions and specialization**
- Out-of-process tool manifest and loader per ADR-0001 (a tool is a directory: command, schema, capability atoms, dependencies materialised at install time); MCP client with lazy exposure over the same launcher; `find_tools`; skills loader; sub-agent spawn with monotone narrowing; a real `ArtifactStore` behind the kernel's spill (D12); notebook-based compaction middleware; `Memory` trait finalized with one baseline implementation.
- First task agents: an orchestrator profile and a scoped "FEA model debugger" profile.
- Exit: debugger sub-agent provably cannot invoke meshing tools; context per turn stays under a set budget on a real simulation task.

**Phase 3 — Provenance and orchestration**
- Projector → SQLite/Postgres PROV schema; artifact metadata; queries for "why this artifact".
- Orchestrator, grown from the P1.9 supervisor (D4): placement (local/podman/runner), wakers (Slurm epilog, file watcher, webhook), trust tiers, agent-to-agent messaging.
- Workflow runner with a mesh → solve → post → viz pipeline as the reference.
- Exit: an end-to-end simulation run whose every product traces back to the checkpoint and profile version that produced it.

**Phase 4 — Evolve loop**
- Proposer profile with archive access; eval runner with hidden sets and parallel-sampling baseline; promotion gates; checkpoint-fork experiments.
- First targets: memory module search, tool-allowlist pruning, compaction thresholds per model profile.
- Exit: one promoted change with held-out gain and a provenance trail, and one correctly *rejected* change that only improved the dev set.

**Phase 5 — Fine-tuned specialists**
- vLLM-served fine-tune with its own model profile (parsed tool format, trained-against hash), eval set, and drift warning.
- Trace export pipeline for training data.

---

## 14. Risk register

| Risk | Consequence | Mitigation |
|---|---|---|
| Extension mechanism chosen wrong | "Self-modifying" becomes recompile-and-restart; agent can't write its own extensions | **Decided (ADR-0001):** process JSON-RPC over stdio; a tool is a directory the agent can write at runtime, no compile step. WASM held in reserve. Residual: SIGTERM delivery under `--unshare-pid` to be verified on the P0.1 stack |
| bwrap won't nest in rootless Podman under the login node's 2002-uid map | No inner sandbox in the environment we care about most | **Still open (ADR-0002 pending):** P0.1 scripts ready, login-node run outstanding; fallbacks in order: bwrap on the login node, namespace-per-container, gVisor, the ADR-0001 WASM route |
| Kernel feature creep | Boundary erodes; self-modification and provenance both become unsafe | Ruthless rule: kernel changes require an ADR; everything else is `ext` |
| Profiles overriding structure | A team profile replaces the loop or widens the sandbox | Validator rejects structural overrides; capabilities only narrow |
| Checkpoint schema drift | Evolve loop can't fork old runs; replay breaks | `schema_version` + migrations from day one |
| Evolve loop overfits / optimizes noise | False "improvements" promoted | Hidden held-out sets, parallel-sampling baseline, process-based sim evals, reviewer gate |
| Fine-tune / harness drift | Prompt tweak silently breaks a model trained on the old prompt | Pin trained-against profile hash; warn on mismatch |
| Middleware ordering bugs | Silent behavior changes | Resolved chain logged into every run |
| Workflow DSL creep | We rebuild LangGraph | The "loops-with-state ⇒ agent profile" rule |
| Context flooding from MCP | Cost and quality collapse on simulation tasks | Lazy exposure, REPL-first, artifact spill, scoped sub-agents |
| Agent writes its own pedigree | Provenance untrustworthy | Harness records at tool boundaries; agent can only annotate |

v0.2: the provider-shape question (one client with quirk flags vs. one client per endpoint) was a risk to P1.5's `Provider` signature; it is decided by ADR-0003 and closed. Per-milestone risk tracking lives in `docs/IMPLEMENTATION_PLAN.md`.

---

## 15. Open questions

1. ~~Extension mechanism (WASM components vs. process-RPC vs. scripting)~~ — **decided, ADR-0001**: process JSON-RPC over stdio; WASM in reserve.
2. ~~Wire protocol: adopt ACP, extend it, or define our own JSON-RPC schema and provide an ACP shim?~~ — **decided, ADR-0004**: adopt ACP v2 on both legs, with a `_grist/*` extension namespace for the log-facing event kinds and the `Tool`/`Task` cancel scopes; Zed is the first client and the Tauri shell moves to P3.
3. Memory module interface: how much of ALMA's search space (schema + retrieval + update code) do we expose vs. constrain?
4. Provenance store at fleet scale: Postgres alone, or a graph layer on top for PROV queries?
5. How aggressively should sub-agent capability pruning be automated vs. proposed-for-review?
6. Which simulation eval set do we author first, and who owns its hidden half? (D19 fixes the mechanism — a private repo mounted only into the eval runner's sandbox, owned by a named person; the set and the person are still open.)
7. Sandbox stack on the HPC login node — **still open, ADR-0002 pending** the P0.1 login-node run (§8).

Decided since v0.1 and no longer open: provider client shape (ADR-0003, §5); the wire protocol and the first client (ADR-0004, §11, §13); the process model, transport, and secrets handling (D4, D10, D11, §3, §11).

---

## Appendix A — Reference list

- pi coding agent (badlogic/pi-mono); Zechner, "What I learned building an opinionated and minimal coding agent" (2025)
- LangChain Deep Agents Code (dcode) docs: profiles, agents, sub-agents, sandboxes
- Amp: Agents in Orbs; agent-to-agent messaging, self-scheduling, event-driven orbs (Jul 2026)
- Lee et al., *Meta-Harness: End-to-End Optimization of Model Harnesses* (2026), arXiv:2603.28052
- Wu et al., *HarnessDev: Can LLMs Create and Evolve Their Own Agent Harness?* (2026), arXiv:2609.01437
- *Harness Evolution for LLM Agents* (Jul 2026), arXiv:2607.12227; *Harness Updating Is Not Harness Benefit* (2026), arXiv:2605.30621
- Xiong, Hu, Clune, *Learning to Continually Learn via Meta-learning Agentic Memory Designs* (ALMA, 2026), arXiv:2602.07755
- Zhang et al., *Darwin Gödel Machine* (ICLR 2026); Hu, Lu, Clune, *Automated Design of Agentic Systems* (ICLR 2025)
- HarnessX (2026), arXiv:2606.14249 — harness/model co-evolution
- Agent Client Protocol (Zed); Model Context Protocol; W3C PROV-DM
