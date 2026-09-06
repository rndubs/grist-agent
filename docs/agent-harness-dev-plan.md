# Agent Harness Development Plan

*A minimal, embeddable, self-improvable agent kernel for simulation and engineering workflows*

Draft v0.1 — September 2026

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
┌──────────────┐   protocol (JSON-RPC / ACP over stdio, socket, websocket)
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

### 3.1 Crate layout

| Crate | Responsibility | Mutable by evolve loop? |
|---|---|---|
| `kernel` | Agent loop, typed `State`, `Tool` trait, `Middleware` trait, event log, checkpoints, suspend/resume | **No** (frozen) |
| `providers` | Model clients. v0: one OpenAI-compatible client with per-endpoint quirk flags (covers vLLM, LiteLLM) | No |
| `host` | Trait for filesystem, process spawn, network, secrets, UI prompts; impls: `native`, `remote-client` | No |
| `ext` | Extension API + first-party extensions: MCP client, sub-agent spawn, skills loader, memory modules, workflow runner, Python REPL tool, permission/capability gate | Extensions yes; API no |
| `profiles` | Loading/merging/validating model profiles, agent profiles, project overrides; catalog | Content yes; loader no |
| `sandbox` | Inner bwrap policy derivation from capability declarations | Policy yes; enforcement no |
| `orchestrator` | Placement, wakers, fleet, agent-to-agent messaging, trust tiers | No |
| `provenance` | Event-log projector → relational schema; artifact store | Schema no |
| `evolve` | Outer loop: proposer harness, eval runner, promotion gates | Yes (it's a profile too) |
| `ui` | Tauri app + web build speaking the protocol | n/a |

**Why this split:** the "frozen kernel / mutable everything else" line is the single most important boundary in the system. It is what makes self-modification safe (the agent cannot edit the loop or the sandbox), makes provenance honest (the harness records, the agent annotates), and defines the search space for evolution (all the mutable files).

---

## 4. The kernel

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

### 4.2 Core types

- **`State`** — typed Rust struct (serde): messages, pending tasks, active profile hashes, memory pointer, notebook path, sandbox policy hash. **Versioned from day one** (`schema_version` field) because the evolve loop will fork old checkpoints.
- **`Tool`** — `name`, `description`, `schema`, `capabilities: Vec<Capability>`, `invoke() -> Result<ToolResult>`. `ToolResult` is `Value | Task { id, status, eta, check_hint }`. Results over a size cap are spilled to the artifact store and replaced by `{handle, head, tail}` automatically.
- **`Middleware`** — `before_model`, `after_model`, `before_tool`, `after_tool`, `on_compact`, `on_resume`. Ordered chain declared in the agent profile; the resolved chain is logged into every run.
- **`Event`** — every message, tool call, tool result, checkpoint, compaction, spawn, suspend, resume, profile load, and harness edit. Append-only JSONL. This log is simultaneously the session file, the checkpoint stream, the replay cassette, and the provenance feed.

### 4.3 Why async tasks live in the kernel

An HPC job is not a slow tool call; it is a tool call that returns a handle. If the kernel understands `Task`, the agent never polls in context, the process can be released for hours, and resumption is an ordinary checkpoint restore triggered by an external waker (Slurm epilog, file watcher, webhook, polling sidecar). Putting this in an extension would force every long-running tool to reinvent it.

### 4.4 Record/replay

Model responses and tool results are recorded keyed by `(checkpoint_hash, request_hash)`. Middleware and profile changes can then be tested against recorded runs in milliseconds with no API spend and no nondeterminism. This is what makes harness iteration feel like normal software engineering, for humans and for the evolve loop.

---

## 5. Providers: vLLM and LiteLLM first

Both expose OpenAI-compatible `/v1/chat/completions` with SSE streaming, so v0 is **one client with quirk flags**, not two providers:

| Concern | vLLM | LiteLLM proxy | Handling |
|---|---|---|---|
| Model naming | bare served name | `provider/model` routed by proxy | Model profile carries the full string; kernel treats it as opaque |
| Tool calling | requires server started with auto-tool-choice + a tool-call parser; behavior varies per model | passes through to upstream; varies per upstream | Model profile declares `tool_format: native | parsed(<syntax>)`; a parser middleware normalizes non-native formats |
| Reasoning/thinking | `reasoning_content` field (model-dependent) | upstream-dependent, sometimes remapped | Provider maps whatever field exists into a `Thinking` content block; serialization preserves it across providers |
| Structured output | JSON schema / guided decoding flags | upstream-dependent | Expose as a provider capability flag; middleware falls back to prompt+parse if absent |
| Auth / base URL | usually none / local | API key + proxy URL | `host.secrets`, per-endpoint config |
| Context length | server-configured | per-model in proxy config | Model profile declares it; compaction middleware reads it |

**Why this order:** fine-tuned and local models will be served by vLLM; LiteLLM gives us every hosted provider through one door and centralizes keys and spend. A native Anthropic/OpenAI client comes later, mainly for prompt caching and provider-specific features.

---

## 6. Profiles and the catalog

Two orthogonal layers over the kernel, both as files, both versioned:

**Model profile** (keyed by `endpoint:model`): system prompt variant, tool-description phrasings, tool-call format, thinking/temperature defaults, context length, compaction thresholds, known quirks, and — for fine-tunes — the profile hash the model was trained against.

**Agent profile** (per team / repo / role): capability bundles enabled (`meshing`, `solver`, `post`, `fs.rw:workdir`, `net:allowlist`), MCP servers (registered lazily), skills, `AGENTS.md`, sub-agent definitions, middleware chain, memory module, sandbox policy, eval set pointer.

Resolution: `kernel defaults + model profile + agent profile + project overrides`. Profiles override *values*, never *structure*; a profile cannot replace the loop or widen the sandbox.

**Task agent** = (model profile, agent profile) pair registered in the catalog. Fine-tuned specialists and generic sub-agents are the same kind of thing; a parent simply spawns by catalog name.

**Sub-agent scoping** is enforced at three levels: (1) the kernel only instantiates allowlisted tools — the model cannot call what isn't registered; (2) the inner sandbox policy is derived from the same list, so bash cannot route around it; (3) `spawn` narrows monotonically — a child's capabilities are a subset of its parent's. A child needing more sends a request up rather than acquiring a tool.

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
- **Inner (bwrap per tool invocation).** Least privilege as the permission model: repo RW, everything else RO, network off unless the capability is granted, tmpfs scratch, timeout. Derived mechanically from the tool's `capabilities` and the profile's grants. Sub-agents get narrower policies than parents.

The inner policy is declarative and may be evolved; the outer never is.

**Known hazard:** bwrap nesting inside rootless Podman requires unprivileged user namespaces and often seccomp/userns adjustments. This exact stack (Podman → outer bwrap → inner bwrap) is validated in Phase 0.

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

- **An agent is a kernel process speaking the protocol over a transport.** Local GUI, remote daemon, and sub-agents are indistinguishable to the UI.
- **Placement** is an orchestrator decision: `local`, `podman`, or a named `runner` (e.g., a cluster login node with the filesystem mounted). Run near the data.
- **Wakers** resume suspended agents: Slurm epilog, file watcher, cron/self-schedule, inbound webhook. Each source maps to an outer-sandbox trust tier.
- **Agent-to-agent messaging** uses the same protocol; external agentic systems are wrapped as tools that return `Task` handles, so a slow collaborator looks exactly like a slow solver.
- **UI**: Tauri app whose web frontend also serves as the remote daemon's browser UI. ACP evaluated as the wire format in Phase 1.

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

**Phase 0 — Spikes (de-risk before design freeze)**
- Podman → outer bwrap → inner bwrap nesting works in the target cluster environment.
- OpenAI-compatible client against vLLM (tool calling + reasoning field) and LiteLLM (routing + auth).
- Extension mechanism decision: WASM Component Model plugins vs. process-based JSON-RPC extensions vs. scripting. Prototype the top two with the Python REPL tool as the test case.
- Exit criteria: all three spikes green; extension mechanism chosen and written up.

**Phase 1 — Kernel + local daemon**
- `kernel`, `providers`, `host::native`, event log, checkpoints, suspend/resume, record/replay.
- Four base tools (read, write, edit, bash) + Python REPL, all through the inner sandbox.
- Profiles (model + agent), catalog, one default agent.
- Minimal protocol server (stdio + socket); CLI-less: a thin Tauri UI or ACP-compatible editor as client.
- Exit: a coding session and a "run a script, suspend, resume on completion" session both replay deterministically from the log.

**Phase 2 — Extensions and specialization**
- MCP client with lazy exposure; `find_tools`; skills loader; sub-agent spawn with monotone narrowing; artifact store with spill; notebook-based compaction middleware; memory module interface with one baseline implementation.
- First task agents: an orchestrator profile and a scoped "FEA model debugger" profile.
- Exit: debugger sub-agent provably cannot invoke meshing tools; context per turn stays under a set budget on a real simulation task.

**Phase 3 — Provenance and orchestration**
- Projector → SQLite/Postgres PROV schema; artifact metadata; queries for "why this artifact".
- Orchestrator: placement (local/podman/runner), wakers (Slurm epilog, file watcher, webhook), trust tiers, agent-to-agent messaging.
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
| Extension mechanism chosen wrong | "Self-modifying" becomes recompile-and-restart; agent can't write its own extensions | Phase 0 spike; favor a mechanism the agent can author at runtime |
| bwrap won't nest in rootless Podman | No inner sandbox in the cloud, the environment we care about most | Phase 0 spike; fall back to namespace-per-container or gVisor if needed |
| Kernel feature creep | Boundary erodes; self-modification and provenance both become unsafe | Ruthless rule: kernel changes require an ADR; everything else is `ext` |
| Profiles overriding structure | A team profile replaces the loop or widens the sandbox | Validator rejects structural overrides; capabilities only narrow |
| Checkpoint schema drift | Evolve loop can't fork old runs; replay breaks | `schema_version` + migrations from day one |
| Evolve loop overfits / optimizes noise | False "improvements" promoted | Hidden held-out sets, parallel-sampling baseline, process-based sim evals, reviewer gate |
| Fine-tune / harness drift | Prompt tweak silently breaks a model trained on the old prompt | Pin trained-against profile hash; warn on mismatch |
| Middleware ordering bugs | Silent behavior changes | Resolved chain logged into every run |
| Workflow DSL creep | We rebuild LangGraph | The "loops-with-state ⇒ agent profile" rule |
| Context flooding from MCP | Cost and quality collapse on simulation tasks | Lazy exposure, REPL-first, artifact spill, scoped sub-agents |
| Agent writes its own pedigree | Provenance untrustworthy | Harness records at tool boundaries; agent can only annotate |

---

## 15. Open questions

1. Extension mechanism (WASM components vs. process-RPC vs. scripting) — Phase 0 decides.
2. Wire protocol: adopt ACP, extend it, or define our own JSON-RPC schema and provide an ACP shim?
3. Memory module interface: how much of ALMA's search space (schema + retrieval + update code) do we expose vs. constrain?
4. Provenance store at fleet scale: Postgres alone, or a graph layer on top for PROV queries?
5. How aggressively should sub-agent capability pruning be automated vs. proposed-for-review?
6. Which simulation eval set do we author first, and who owns its hidden half?

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
