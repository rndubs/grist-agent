# Implementation Plan — Agent Harness

Derived from [`agent-harness-dev-plan.md`](./agent-harness-dev-plan.md) (Draft v0.1, September 2026).
That document is the *why*; this one is the *what, in what order, and how we know it's done*.

This file is the single source of truth for progress. Update it in the same PR as the work it tracks.

---

## How to use this document

- **Phases** follow §13 of the dev plan. A phase is done only when every item in its *Exit criteria* block is checked and verified, not when its milestones are merely merged.
- **Milestones** carry stable IDs (`P1.3`, `P2.7`, …). Reference them in branch names, PR titles, and ADRs.
- **Tasks** are checkboxes under each milestone. Tick them when the work is merged to `main`, not when it is started.
- **Milestone status** is one of `not started`, `in progress`, `blocked`, `done`. Set it in the milestone header line. If `blocked`, say on what.
- **Kernel boundary rule (§14):** any change to `kernel` after P1 requires an ADR in `docs/adr/`. Anything else goes in `ext`, a profile, or a policy file.
- **Nothing is promoted on faith (§1, goal 4):** a milestone that claims a behavioral property (e.g., "child cannot call meshing tools") is done only when a test asserts that property.

### Status legend

| Symbol | Meaning |
|---|---|
| `[ ]` | not started |
| `[x]` | merged to `main` and verified |
| ⏸ | blocked (explain in header) |

---

## Progress summary

| Phase | Name | Status | Milestones done | Exit criteria met |
|---|---|---|---|---|
| P0 | Spikes (de-risk before design freeze) | not started | 0 / 5 | no |
| P1 | Kernel + local daemon | not started | 0 / 9 | no |
| P2 | Extensions and specialization | not started | 0 / 9 | no |
| P3 | Provenance and orchestration | not started | 0 / 7 | no |
| P4 | Evolve loop | not started | 0 / 7 | no |
| P5 | Fine-tuned specialists | not started | 0 / 3 | no |

Dependency order is strict between phases (P0 → P1 → P2 → P3 → P4 → P5) except where a milestone notes otherwise. Within a phase, milestones are listed in a sensible build order, but parallel work is fine where no dependency is stated.

---

## Phase 0 — Spikes (de-risk before design freeze)

**Purpose:** validate the three assumptions the whole design rests on before writing any kernel code. Spike code is throwaway; the *decisions* are the deliverable.

### P0.0 — Repository foundations — `not started`

*Not in the dev plan explicitly; needed so spike results land somewhere real.*

- [ ] Cargo workspace with stub crates matching §3.1: `kernel`, `providers`, `host`, `ext`, `profiles`, `sandbox`, `orchestrator`, `provenance`, `evolve` (each compiles, exports nothing)
- [ ] `spikes/` directory for throwaway P0 code, excluded from the workspace build
- [ ] CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on every PR
- [ ] Toolchain pinned (`rust-toolchain.toml`); MSRV recorded
- [ ] ADR template at `docs/adr/0000-template.md`; ADR index in `docs/adr/README.md`
- [ ] `CONTRIBUTING.md` stating the kernel-boundary rule and the "tick on merge" convention

### P0.1 — Sandbox nesting spike — `not started`

Risk addressed: *bwrap won't nest in rootless Podman* (§14). This is the environment we care most about.

- [ ] Rootless Podman container image with `bwrap` installed
- [ ] Outer `bwrap` wrapping a trivial process inside the container
- [ ] Inner `bwrap` launched from inside the outer sandbox (per-tool-call shape: repo RW, rest RO, tmpfs scratch, net off, timeout)
- [ ] Document required host settings (unprivileged user namespaces, seccomp / userns adjustments)
- [ ] Run the same stack in the *target cluster environment*, not just a dev laptop
- [ ] If nesting fails: evaluate fallbacks named in §14 (namespace-per-container, gVisor) and record the result
- [ ] Write-up: `docs/spikes/sandbox-nesting.md` with exact commands, kernel/Podman versions, and outcome

### P0.2 — Provider spike (vLLM + LiteLLM) — `not started`

Validates the "one client with quirk flags" decision in §5.

- [ ] Minimal OpenAI-compatible `/v1/chat/completions` client with SSE streaming
- [ ] vLLM: tool calling works with auto-tool-choice + a tool-call parser; record which models need `parsed(<syntax>)` handling
- [ ] vLLM: `reasoning_content` (or equivalent) captured and mapped to a `Thinking` content block
- [ ] LiteLLM proxy: `provider/model` routing works; API key + base URL sourced from config, not hardcoded
- [ ] LiteLLM: reasoning/thinking field behavior for at least one upstream recorded
- [ ] Structured-output capability probed on both (JSON schema / guided decoding) and recorded as a per-endpoint flag
- [ ] Table of quirk flags needed, feeding the `providers` crate design (P1.4)
- [ ] Write-up: `docs/spikes/providers.md`

### P0.3 — Extension mechanism spike — `not started`

Risk addressed: *extension mechanism chosen wrong* (§14). Open question §15.1. Test case: the Python REPL tool (§7.1), because it is stateful, long-lived, and must run inside the inner sandbox.

- [ ] Shortlist the three candidates: WASM Component Model plugins, process-based JSON-RPC extensions, embedded scripting
- [ ] Prototype candidate A with the Python REPL tool as a persistent, session-scoped kernel
- [ ] Prototype candidate B with the same tool
- [ ] Score both against: runtime authorability by the agent (can it write its own extension without a recompile?), sandbox compatibility (works under P0.1's inner bwrap), state persistence across calls, latency per call, packaging/distribution story
- [ ] Write-up: `docs/spikes/extension-mechanism.md`

### P0.4 — Decisions and design freeze — `not started`

- [ ] **ADR-0001** Extension mechanism (from P0.3)
- [ ] **ADR-0002** Sandbox stack and fallback (from P0.1)
- [ ] **ADR-0003** Provider client shape: one OpenAI-compatible client with quirk flags (from P0.2)
- [ ] Update the dev plan (§3, §4, §8) with anything the spikes changed; bump to v0.2
- [ ] Freeze the crate list and the `kernel` public surface for P1

### Exit criteria — Phase 0

- [ ] Sandbox spike green in the target cluster environment (or a fallback chosen and documented)
- [ ] Provider spike green against both vLLM and LiteLLM, including tool calling and the reasoning field
- [ ] Extension mechanism chosen and written up as ADR-0001
- [ ] All three write-ups merged under `docs/spikes/`

---

## Phase 1 — Kernel + local daemon

**Purpose:** the frozen core (§4) plus enough around it to run a real session locally and replay it. After this phase, `kernel` changes require an ADR.

### P1.1 — Core types — `not started`

- [ ] `State` struct (serde) with `schema_version` from the first commit; fields: messages, pending tasks, active profile hashes, memory pointer, notebook path, sandbox policy hash
- [ ] `State` migration hook: loading an older `schema_version` runs a registered migration or fails loudly
- [ ] `Tool` trait: `name`, `description`, `schema`, `capabilities: Vec<Capability>`, `invoke() -> Result<ToolResult>`
- [ ] `ToolResult = Value | Task { id, status, eta, check_hint }`
- [ ] `Capability` enum / type covering at least: `fs.ro`, `fs.rw:<scope>`, `net:<allowlist>`, `proc.spawn`, plus domain bundles (`meshing`, `solver`, `post`) as opaque names
- [ ] `Middleware` trait: `before_model`, `after_model`, `before_tool`, `after_tool`, `on_compact`, `on_resume`
- [ ] `Event` enum covering every event type named in §4.2: message, tool call, tool result, checkpoint, compaction, spawn, suspend, resume, profile load, harness edit
- [ ] Unit tests for serde round-trips of every core type

### P1.2 — The loop — `not started`

- [ ] Implement the §4.1 loop exactly as written: middleware hooks in the stated order, checkpoint after each turn, suspend when only pending tasks remain, break on done
- [ ] Ordered middleware chain, constructed from a list; the *resolved* chain is written to the event log at run start (§14, ordering bugs)
- [ ] Tool registry: the loop can only invoke tools that were registered at construction (enforcement level 1 of §6)
- [ ] Loop tests with a fake provider and fake tools covering: plain turn, tool call turn, multi-tool turn, suspend on pending task, done

### P1.3 — Event log and checkpoints — `not started`

- [ ] Append-only JSONL writer; one file per session; never rewritten
- [ ] Checkpoint = event carrying a hash of `State` plus enough to restore it
- [ ] `restore(checkpoint_hash) -> State`
- [ ] Suspend: persist state, release the process; Resume: restore from the latest checkpoint and continue the loop (waker integration is P3.4; for now a manual `resume` call)
- [ ] Session file is readable as a plain log (this is the "session format as foundational contract" from §2)
- [ ] Test: run → suspend → resume produces the same event sequence as an uninterrupted run

### P1.4 — Record/replay — `not started`

- [ ] Recorder middleware captures model responses and tool results keyed by `(checkpoint_hash, request_hash)`
- [ ] Replay provider and replay tool-invoker serve recorded responses; a cache miss is an error, not a live call
- [ ] Deterministic `request_hash` definition documented (what is and isn't included)
- [ ] Test: a recorded session replays in under a second with zero network access

### P1.5 — `providers` crate — `not started`

Depends on P0.2 / ADR-0003.

- [ ] One OpenAI-compatible client with SSE streaming
- [ ] Per-endpoint quirk flags from the P0.2 table (tool format, reasoning field name, structured-output support, auth mode)
- [ ] Reasoning/thinking mapped into a `Thinking` content block that survives serialization
- [ ] `Provider` trait so replay (P1.4) and future native clients plug in identically
- [ ] Integration tests against vLLM and LiteLLM (gated behind an env var; skipped in CI without endpoints)

### P1.6 — `host` crate — `not started`

- [ ] `Host` trait: filesystem, process spawn, network, secrets, UI prompts
- [ ] `host::native` implementation
- [ ] `host::remote-client` left as a stub with a documented interface (filled in P3)
- [ ] Kernel and tools take `&dyn Host`; nothing in `kernel` touches `std::fs` or `std::process` directly

### P1.7 — `sandbox` crate and base tools — `not started`

Depends on P0.1 / ADR-0002.

- [ ] Inner policy type: mounts (RW/RO), tmpfs scratch, network on/off + allowlist, timeout
- [ ] `derive_policy(tool.capabilities, profile.grants) -> Policy`, purely mechanical, no escape hatch
- [ ] bwrap launcher that applies a `Policy` to a single tool invocation
- [ ] Base tools: `read`, `write`, `edit`, `bash`, each declaring capabilities and running through the inner sandbox
- [ ] Python REPL tool per ADR-0001: persistent session-scoped interpreter inside the inner sandbox
- [ ] Tests: a tool with `fs.ro` cannot write; a tool without `net` cannot open a socket; a timeout is enforced

### P1.8 — `profiles` crate and catalog — `not started`

- [ ] Model profile schema (§6): system prompt variant, tool-description phrasings, `tool_format: native | parsed(<syntax>)`, thinking/temperature defaults, context length, compaction thresholds, quirks, optional `trained_against_profile_hash`
- [ ] Agent profile schema (§6): capability bundles, MCP servers, skills, `AGENTS.md`, sub-agent definitions, middleware chain, memory module, sandbox policy, eval set pointer
- [ ] Project overrides file
- [ ] Resolution: `kernel defaults + model profile + agent profile + project overrides`
- [ ] **Validator:** profiles override values, never structure; capabilities may only narrow; a profile cannot name a middleware or tool that is not registered. Tests for each rejection.
- [ ] Every loaded profile is content-hashed; hashes are recorded in `State` and in a `profile load` event
- [ ] Catalog: registry of task agents = (model profile, agent profile) pairs, addressable by name
- [ ] One default agent (four base tools + Python REPL) shipped in-repo

### P1.9 — Protocol server and first client — `not started`

Open question §15.2 (wire protocol) is decided here.

- [ ] Evaluate Agent Client Protocol (ACP) as the wire format; **ADR-0004** records adopt / extend / own-schema-with-ACP-shim
- [ ] JSON-RPC server over stdio and unix socket; websocket left for P3
- [ ] Protocol covers: start session, send user message, stream events, list/approve nothing (no permission popups by design, §1 goal 6), suspend, resume, replay
- [ ] First client: either a thin Tauri shell or an ACP-compatible editor, whichever ADR-0004 makes cheaper. No CLI (§13, "CLI-less")
- [ ] Every protocol message maps to or from an `Event`; no protocol-only state

### Exit criteria — Phase 1

- [ ] A coding session (read/edit/bash on a real repo) is recorded and then replays deterministically from its log with no network
- [ ] A "run a script, suspend, resume on completion" session is recorded and replays deterministically
- [ ] `kernel` public API frozen; kernel-boundary rule now in force (ADR required)
- [ ] `State.schema_version` migrations exercised by at least one test

---

## Phase 2 — Extensions and specialization

**Purpose:** everything that makes an agent *specific*, built on the extension API third parties will use (§1 goal 2). Context discipline defaults from §7 land here.

### P2.1 — Extension API — `not started`

Depends on ADR-0001.

- [ ] Public extension API in `ext` matching the chosen mechanism; first-party extensions use only this API (no private hooks into `kernel`)
- [ ] Extension manifest: name, version, tools provided, middleware provided, capabilities required
- [ ] Loading extensions declared in an agent profile; a profile cannot load an extension that requests capabilities beyond the profile's grants
- [ ] Example third-party extension in `examples/` that adds one tool and one middleware

### P2.2 — MCP client with lazy exposure — `not started`

- [ ] MCP client (stdio and HTTP transports) as an `ext` module
- [ ] Servers register at session start but their tool schemas are **not** put in the prompt
- [ ] `find_tools(query)` tool surfaces relevant MCP tools for the next turn only
- [ ] Skills can reference MCP tools by name, which exposes them for that turn
- [ ] MCP tool calls run through the inner sandbox policy derived from the server's declared capabilities
- [ ] Test: prompt token count with N registered servers is independent of N

### P2.3 — Skills loader — `not started`

- [ ] Skill = markdown with frontmatter (name, description, tools referenced, capabilities required)
- [ ] Loaded from paths named in the agent profile; content-hashed and logged as a `profile load` event
- [ ] Skill invocation injects the skill body into context for the turn

### P2.4 — Sub-agent spawn with monotone narrowing — `not started`

- [ ] Sub-agent definitions as markdown-with-frontmatter (from dcode, §2), referencing a catalog entry
- [ ] `spawn(catalog_name, task)` tool returning a `Task` handle
- [ ] Enforcement level 1: child kernel is constructed with only the child's allowlisted tools
- [ ] Enforcement level 2: child inner sandbox policy derived from that same list
- [ ] Enforcement level 3: child capabilities must be a subset of parent's; spawn is rejected otherwise
- [ ] `request_capability` message from child to parent (the child asks; it never acquires)
- [ ] `spawn` and child completion recorded as events in the parent log with a link to the child log

### P2.5 — Artifact store with spill — `not started`

- [ ] Content-addressed store (hash → bytes) with a local filesystem backend
- [ ] `after_tool` middleware: results over a configurable cap are stored and replaced by `{ handle, head, tail }`
- [ ] `read_artifact(handle, range)` tool
- [ ] Solver-log structured-extraction pass (errors, convergence, timings) as a first domain-specific spill handler
- [ ] Visualization results returned as PNG artifacts, not inline data

### P2.6 — Notebook-based compaction middleware — `not started`

- [ ] Lab-notebook file path in `State`; harness re-injects it in `on_resume`
- [ ] `on_compact` summarizes *toward the notebook* (updates the notebook, then trims context), not into a lossy paragraph
- [ ] Compaction thresholds read from the model profile's context length
- [ ] Compaction recorded as an event with before/after hashes
- [ ] Test: replay a long session with and without compaction; notebook content is preserved

### P2.7 — Memory module interface — `not started`

Open question §15.3 is decided here (**ADR-0005**: how much of ALMA's search space is exposed).

- [ ] `Memory` trait: `store`, `retrieve`, `update`, `compress`, `forget`
- [ ] Memory pointer in `State`; memory snapshots content-addressed
- [ ] One baseline implementation (e.g., file-backed notes with keyword retrieval)
- [ ] Memory module selected by agent profile; the module is data the evolve loop may replace

### P2.8 — First task agents — `not started`

- [ ] Orchestrator profile: broad capabilities, may spawn, owns the notebook
- [ ] "FEA model debugger" profile: scoped to `solver` + `post` + `fs.rw:workdir`; **no** `meshing`
- [ ] Both registered in the catalog with model profiles for at least one vLLM-served and one LiteLLM-routed model

### P2.9 — Context budget instrumentation — `not started`

- [ ] Per-turn context token count emitted as an event
- [ ] Budget declared in the agent profile; exceeding it is a warning event (not a failure, but visible)
- [ ] A real simulation task (shared with P3.6's reference pipeline) used as the measurement workload

### Exit criteria — Phase 2

- [ ] A test proves the FEA debugger sub-agent cannot invoke meshing tools at all three enforcement levels (registry, sandbox, spawn narrowing)
- [ ] Context per turn stays under the profile's budget across a full real simulation task, measured via P2.9
- [ ] All first-party extensions (MCP, spawn, skills, memory, artifact spill) are built on the P2.1 API and nothing else
- [ ] ADR-0005 (memory interface scope) merged

---

## Phase 3 — Provenance and orchestration

**Purpose:** make the agent traceable like any other data-producing process (§10) and runnable near the data (§11).

### P3.1 — Provenance projector — `not started`

- [ ] W3C PROV-DM schema in SQLite: `Entity`, `Activity`, `Agent`; edges `used`, `generated`, `wasDerivedFrom`, `wasAttributedTo`, `wasInformedBy`
- [ ] Agent identity = `(model_id, model_profile_hash, agent_profile_hash, kernel_version)`
- [ ] Projector folds the JSONL event log into the schema; idempotent; re-runnable from scratch
- [ ] Every tool boundary emits pedigree rows mechanically; the agent has no write path to the graph
- [ ] Agent annotations (rationale, hypotheses) stored as entities `wasAttributedTo` the agent, never as graph edits
- [ ] Profile changes produce a *new* agent node with a `wasDerivedFrom` edge (§10 rule 2)
- [ ] Postgres backend behind the same interface (fleet scale; open question §15.4 → **ADR-0006**)

### P3.2 — Artifact metadata and "why this artifact" queries — `not started`

- [ ] Artifact store (P2.5) entries projected as `Entity` rows with hash, size, type, producing activity
- [ ] Query: artifact hash → producing tool call → checkpoint → reasoning trace, skill, memory snapshot (the "why is this mesh 2 mm?" path from §10)
- [ ] Query: per-profile tool usage statistics, sub-agent request patterns, eval outcomes (inputs to P4)
- [ ] Exposed over the protocol so the UI can show a provenance view

### P3.3 — Orchestrator: placement — `not started`

- [ ] `orchestrator` crate: launch a kernel process with a placement of `local`, `podman`, or a named `runner`
- [ ] Outer bwrap applied by the launcher, outside the mutable layer; the harness cannot see or alter it
- [ ] `host::remote-client` (from P1.6 stub) so a remote kernel's host calls reach the right filesystem
- [ ] Websocket transport added to the protocol server; the same UI talks to local and remote kernels indistinguishably

### P3.4 — Wakers and trust tiers — `not started`

- [ ] Waker interface: "resume session X because Y"
- [ ] Wakers: Slurm epilog hook, file watcher, cron / self-schedule, inbound webhook, polling sidecar
- [ ] Trust tier per trigger source (interactive > scheduled > inbound webhook) selects the outer sandbox tier
- [ ] Resume event records the waker source and tier
- [ ] Test: a `Task`-returning tool + a file-watcher waker completes a suspend/resume cycle with no polling in context

### P3.5 — Agent-to-agent messaging — `not started`

- [ ] Messaging over the same protocol as UI ↔ kernel
- [ ] External agentic systems wrapped as tools that return `Task` handles
- [ ] Inbound messages carry a trust tier like any other trigger

### P3.6 — Workflow runner — `not started`

- [ ] Workflow schema in TOML: nodes are `{ task agent | tool | pure function | sub-workflow }`; edges are static or routing functions
- [ ] Small Rust builder for cases needing real code
- [ ] Runner executes a workflow, checkpointing per node; each node run is an `Activity` in provenance
- [ ] Reference pipeline: mesh → setup → solve → post → viz, using the P2.8 task agents
- [ ] Guardrail documented and enforced in review: a node that needs loops-with-state becomes an agent profile, not a workflow feature (§9)
- [ ] Checkpoint forking: "replay run N from turn M with a different middleware chain" as a runner operation

### P3.7 — Fleet basics — `not started`

- [ ] List / inspect / suspend / resume running agents across placements
- [ ] Session logs and artifacts from remote placements retrievable locally

### Exit criteria — Phase 3

- [ ] An end-to-end simulation run (reference pipeline) completes with each product traceable to the checkpoint and profile version that produced it, via P3.2 queries
- [ ] The same run works with the kernel placed in `podman`, resumed by a real waker
- [ ] Agent has no write path into the provenance graph (test asserts the projector ignores agent-authored pedigree)
- [ ] ADR-0006 (provenance store at fleet scale) merged

---

## Phase 4 — Evolve loop

**Purpose:** make the harness a searchable object with promotion gates that reject overfitting (§12). Design principle: the outer loop stays deliberately dumb; the proposer has full raw traces.

### P4.1 — Archive exposure — `not started`

- [ ] Filesystem view over event logs + artifact store + provenance DB that a proposer agent can `grep`
- [ ] Read-only mount into the proposer's inner sandbox
- [ ] Index of prior candidates, their diffs, their scores

### P4.2 — Eval runner — `not started`

- [ ] Eval set format: task, inputs, process-based judge (correct setup, convergence, sane post-processing, tolerances), never exact numbers for simulation tasks (§12.6)
- [ ] Each profile points at a visible dev set and a **hidden** held-out set; the proposer cannot read the held-out set (enforced by sandbox policy, tested)
- [ ] Parallel-sampling baseline: same model, same token budget, N samples, best-of-N by the same judge
- [ ] Reports success *and* token efficiency side by side
- [ ] Runs candidates via record/replay first (P1.4) and checkpoint forking (P3.6); live runs are Podman-per-candidate
- [ ] First simulation eval set authored; hidden-half ownership recorded (open question §15.6)

### P4.3 — Promotion gates — `not started`

- [ ] Gate 1: held-out gain over the current profile
- [ ] Gate 2: no regression on any *other* profile's eval set
- [ ] Gate 3: beats the parallel-sampling baseline at equal token cost
- [ ] Gate 4: diff reviewed (human or reviewer-agent) and the review is provenance-linked
- [ ] Gate 5: inner sandbox policy changes pass a static check (may only narrow; never touches the outer sandbox)
- [ ] Gate results stored as provenance entities on the candidate agent node

### P4.4 — Proposer profile — `not started`

- [ ] Agent profile with archive access (P4.1), no network, no eval-set write access
- [ ] Search space limited by allowlist to the mutable layer: prompts, skills, tool descriptions, middleware chains, workflows, memory modules, inner sandbox policy, tool allowlists. Kernel and outer sandbox are unreachable by construction.
- [ ] Output = a candidate profile diff + a written hypothesis naming the concrete failure mode it fixes

### P4.5 — Evolve workflow — `not started`

- [ ] The outer loop itself is a P3.6 workflow: propose → replay-eval → live-eval → gates → promote or reject
- [ ] Promotion creates a new catalog entry with `wasDerivedFrom` the previous one; the old one is never deleted
- [ ] Rejection is recorded with which gate failed

### P4.6 — First targets — `not started`

- [ ] Memory module search (ALMA-style) over P2.7 implementations
- [ ] Tool-allowlist pruning per profile, driven by P3.2 usage statistics; open question §15.5 (automate vs. propose-for-review) → **ADR-0007**
- [ ] Compaction thresholds per model profile

### P4.7 — Training-data closure — `not started`

- [ ] Export successful, profile-labelled traces in a fine-tuning-ready format (feeds P5.3)

### Exit criteria — Phase 4

- [ ] One change promoted with a held-out gain and a complete provenance trail from candidate to catalog entry
- [ ] One change correctly *rejected* that improved the dev set but not the held-out set (demonstrates gate 1 working)
- [ ] The proposer demonstrably cannot read the held-out set or modify `kernel` / outer sandbox (tests)
- [ ] ADR-0007 merged

---

## Phase 5 — Fine-tuned specialists

**Purpose:** harness evolution and weight training share one source; the harness must not silently drift out from under a fine-tune (§14).

### P5.1 — Fine-tune model profile — `not started`

- [ ] Model profile for a vLLM-served fine-tune with `tool_format: parsed(<syntax>)` and `trained_against_profile_hash`
- [ ] Drift warning: loading a fine-tune whose trained-against hash does not match the current resolved profile emits a warning event and is visible in the UI
- [ ] Parser middleware for the fine-tune's tool-call syntax

### P5.2 — Specialist eval set — `not started`

- [ ] Eval set (dev + hidden) specific to the specialist's role, using the P4.2 format
- [ ] Specialist registered in the catalog as a task agent; spawnable by the orchestrator profile like any other

### P5.3 — Trace export pipeline — `not started`

- [ ] P4.7 export productionized: filter by profile, outcome, and eval score; strip secrets; content-hash the export as a provenance entity
- [ ] Round trip documented: traces → fine-tune → new model profile → drift check

### Exit criteria — Phase 5

- [ ] A fine-tuned specialist runs under its own profile, passes its eval set, and its drift warning fires when the profile is changed
- [ ] A training-data export is produced from real traces and is provenance-linked

---

## Cross-cutting tracks

These have no phase; they are checked at every phase boundary.

### Kernel boundary discipline

- [ ] `CONTRIBUTING.md` rule in place (P0.0)
- [ ] CI check or CODEOWNERS on `crates/kernel/` requiring an ADR link in the PR (from P1 exit onward)
- [ ] `kernel` has no dependency on `ext`, `profiles`, `sandbox`, `orchestrator`, `provenance`, or `evolve` (enforced by workspace dependency graph; `cargo deny` or a test)

### Schema versioning

- [ ] `State.schema_version` (P1.1), event schema version, profile schema version, PROV schema version each bumped with a migration and a test

### Documentation

- [ ] `docs/adr/` index current
- [ ] Each crate has a `README.md` stating its responsibility and whether the evolve loop may mutate it (§3.1 table)
- [ ] Dev plan revised at each phase exit (v0.2 after P0, v0.3 after P1, …)

---

## Risk register tracking

Maps §14 risks to the milestones that mitigate them. A risk is *retired* when its mitigating milestones are done and the property is tested.

| Risk | Mitigating milestones | Status |
|---|---|---|
| Extension mechanism chosen wrong | P0.3, P0.4 (ADR-0001), P2.1 | open |
| bwrap won't nest in rootless Podman | P0.1, P0.4 (ADR-0002), P1.7 | open |
| Kernel feature creep | P0.0 (CONTRIBUTING), P1 exit, cross-cutting boundary track | open |
| Profiles overriding structure | P1.8 validator | open |
| Checkpoint schema drift | P1.1 `schema_version` + migration hook, schema versioning track | open |
| Evolve loop overfits / optimizes noise | P4.2 hidden sets + baseline, P4.3 gates, P4 exit "correctly rejected" | open |
| Fine-tune / harness drift | P5.1 trained-against hash + warning | open |
| Middleware ordering bugs | P1.2 resolved chain logged per run | open |
| Workflow DSL creep | P3.6 guardrail | open |
| Context flooding from MCP | P2.2 lazy exposure, P2.5 spill, P2.6 compaction, P2.9 budget | open |
| Agent writes its own pedigree | P3.1 mechanical emission, P3 exit test | open |

---

## Decision log (ADR index)

| ADR | Decision | Decided in | Status |
|---|---|---|---|
| ADR-0001 | Extension mechanism (WASM components vs. process-RPC vs. scripting) | P0.4 | pending |
| ADR-0002 | Sandbox stack (Podman → outer bwrap → inner bwrap) and fallback | P0.4 | pending |
| ADR-0003 | One OpenAI-compatible provider client with quirk flags | P0.4 | pending |
| ADR-0004 | Wire protocol: adopt ACP, extend it, or own JSON-RPC + ACP shim | P1.9 | pending |
| ADR-0005 | Memory module interface: how much of ALMA's search space is exposed | P2.7 | pending |
| ADR-0006 | Provenance store at fleet scale: Postgres alone or graph layer | P3.1 | pending |
| ADR-0007 | Sub-agent capability pruning: automated vs. proposed-for-review | P4.6 | pending |

## Open questions tracker

From §15 of the dev plan.

| # | Question | Resolves in | Status |
|---|---|---|---|
| 1 | Extension mechanism | P0.3 → ADR-0001 | open |
| 2 | Wire protocol (ACP vs. own) | P1.9 → ADR-0004 | open |
| 3 | Memory module interface scope | P2.7 → ADR-0005 | open |
| 4 | Provenance store at fleet scale | P3.1 → ADR-0006 | open |
| 5 | Capability pruning automation | P4.6 → ADR-0007 | open |
| 6 | First simulation eval set and hidden-half ownership | P4.2 | open |

---

## Change log for this plan

| Date | Change |
|---|---|
| 2026-09-06 | Initial plan derived from dev plan v0.1 |
