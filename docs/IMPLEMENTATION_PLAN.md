# Implementation Plan — Agent Harness

Derived from [`agent-harness-dev-plan.md`](./agent-harness-dev-plan.md) (Draft v0.1, September 2026), amended by the twenty decisions in [`design-decisions.md`](./design-decisions.md) (D1–D20).
The dev plan is the *why*; the decisions are the *settled how*; this file is the *what, in what order, and how we know it's done*.

This file is the single source of truth for progress. Update it in the same PR as the work it tracks.

---

## How to use this document

- **Phases** follow §13 of the dev plan. A phase is done only when every item in its *Exit criteria* block is checked and verified, not when its milestones are merely merged.
- **Milestones** carry stable IDs (`P1.3`, `P2.7`, …). Reference them in branch names, PR titles, and ADRs.
- **Tasks** are checkboxes under each milestone. Tick them when the work is merged to `main`, not when it is started.
- **Milestone status** is one of `not started`, `in progress`, `blocked`, `done`. Set it in the milestone header line. If `blocked`, say on what.
- **Decisions are binding.** A task tagged `(D7)` must honor decision D7 as written. An agent that finds a decision unworkable stops and opens an ADR; it does not improvise.
- **Kernel boundary rule (§14, D12):** kernel changes after P1 exit require an ADR; after P2 exit they are exceptional.
- **Nothing is promoted on faith (§1, goal 4):** a milestone that claims a behavioral property is done only when a test asserts that property.

### Status legend

| Symbol | Meaning |
|---|---|
| `[ ]` | not started |
| `[x]` | merged to `main` and verified |
| ⏸ | blocked (explain in header) |
| 🧑 | needs a human (access, review, or infrastructure); agents cannot complete it alone |

---

## Progress summary

| Phase | Name | Status | Milestones done | Exit criteria met |
|---|---|---|---|---|
| P0 | Spikes (de-risk before design freeze) | in progress | 3 / 6 (P0.0, P0.3, P0.5); P0.1, P0.2, P0.4 wait on humans and the login node | 2 / 5 |
| P1 | Kernel + local daemon | in progress | 9 / 10 done (P1.0–P1.8); P1.9 not started | 4 / 5 (soft freeze is a 🧑 decision at exit) |
| P2 | Extensions and specialization | not started | 0 / 9 | no |
| P3 | Provenance and orchestration | not started | 0 / 7 | no |
| P4 | Evolve loop | not started | 0 / 7 | no |
| P5 | Fine-tuned specialists | not started | 0 / 3 | no |

Dependency order is strict between phases (P0 → P1 → P2 → P3 → P4 → P5) except where a milestone notes otherwise. Within a phase, milestones are listed in a sensible build order, but parallel work is fine where no dependency is stated.

---

## Phase 0 — Spikes (de-risk before design freeze)

**Purpose:** validate the assumptions the design rests on before writing kernel code. Spike code is throwaway; the *decisions* and the CI stand-in stack are the deliverables.

### P0.0 — Repository foundations — `done`

- [x] Cargo workspace with stub crates matching §3.1: `kernel`, `providers`, `host`, `ext`, `profiles`, `sandbox`, `orchestrator`, `provenance`, `evolve` (each compiles, exports nothing)
- [x] Workspace dependency DAG written down in `crates/README.md`: `kernel` depends on nothing in-repo; `ext`, `profiles`, `sandbox`, `provenance`, `orchestrator`, `evolve` depend on `kernel`; nothing depends on `evolve`
- [x] `spikes/` directory for throwaway P0 code, excluded from the workspace build
- [x] CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on every PR
- [x] Toolchain pinned (`rust-toolchain.toml`); MSRV recorded; tokio chosen as the async runtime (D4)
- [x] ADR template at `docs/adr/0000-template.md`; ADR index in `docs/adr/README.md`
- [x] `CONTRIBUTING.md` stating the kernel-boundary rule, the tick-on-merge convention, and that `design-decisions.md` is binding

### P0.1 — Sandbox nesting spike on the HPC login node — `in progress` 🧑 (scripts and write-up template ready in `spikes/sandbox-nesting/` and rehearsed once under a local rootless Podman machine on 2026-09-06, which fixed four script bugs and added the `unmask` / `label-disable` variants; every run below still needs the login node, and nothing from the rehearsal counts)

Risk addressed: *bwrap won't nest in rootless Podman* (§14). Target per D18: rootless Podman on the HPC login node under the site's constrained uid map.

- [ ] 🧑 Confirm login-node access and that `podman run --uidmap 0:0:2000 --uidmap 65534:2000:2` works for a trivial image
- [ ] Container image with `bwrap` installed, built with the site's `--userns-uid-map` flags
- [ ] Outer `bwrap` wrapping a trivial process inside the container
- [ ] Inner `bwrap` launched from inside the outer sandbox (per-tool-call shape: repo RW, rest RO, tmpfs scratch, net off, timeout)
- [ ] Record precisely what breaks, if anything: the 2002-uid map, Podman's default seccomp profile, `--userns` mode, `/proc` mounts
- [ ] Fallback test: `bwrap` directly on the login node with no container, since unprivileged user namespaces are evidently enabled there
- [ ] If neither works: evaluate fallbacks named in §14 (namespace-per-container, gVisor) and record the result
- [ ] Write-up: `docs/spikes/sandbox-nesting.md` with exact commands, kernel/Podman/bwrap versions, and outcome
- [ ] OpenShift placement is **not** spiked here; it is in the Backlog (D11, D18)

### P0.2 — Provider spike (vLLM + LiteLLM) — `in progress` 🧑 (client, fake shapes, real-LiteLLM run, and write-up done; real vLLM and hosted-LiteLLM runs need a human; stand-in run happens in the `standin` CI job)

Validates the "one client with quirk flags" decision in §5.

- [x] Minimal OpenAI-compatible `/v1/chat/completions` client with SSE streaming (`spikes/provider-client/`, 11 tests)
- [ ] 🧑 vLLM endpoint available: tool calling works with auto-tool-choice + a tool-call parser; record which models need `parsed(<syntax>)` handling
- [x] vLLM: `reasoning_content` (or equivalent) captured and mapped to a `Thinking` content block (verified against the fake vLLM shape; 🧑 confirm on a real endpoint)
- [ ] 🧑 LiteLLM proxy with keys: `provider/model` routing works; API key + base URL sourced from config, not hardcoded
- [x] LiteLLM: reasoning/thinking field behavior for at least one upstream recorded (real LiteLLM 1.100.0 proxy in front of a vLLM-shaped upstream)
- [x] Structured-output capability probed on both (JSON schema / guided decoding) and recorded as a per-endpoint flag (probe built; cells marked 🧑 until run on real endpoints)
- [x] Same client run against the P0.5 llama.cpp stand-in; differences recorded as quirk flags too (proven in the opt-in `standin` CI job on PR #2; rows marked `verified (CI)` in `docs/spikes/providers.md` §4)
- [x] Table of quirk flags needed, feeding the `providers` crate design (P1.5)
- [x] Write-up: `docs/spikes/providers.md`

### P0.3 — Out-of-process tool mechanism spike — `done` (recommendation: process JSON-RPC; the bwrap-compatibility row is reasoned, not executed, and is re-checked by P0.1)

Scope narrowed by D8: middleware and first-party tools are compiled Rust; this spike chooses only the mechanism for agent-authorable, out-of-process tools. Test case: the Python REPL as a `Session`-kind tool (D5), because it is stateful, long-lived, and must run inside the inner sandbox.

- [x] Prototype A: process-based JSON-RPC tool (long-lived sandboxed process, calls as RPC)
- [x] Prototype B: WASM Component Model tool hosting the same REPL (componentize-py + wasmtime 47)
- [x] Score both against: authorability by the agent at runtime without a recompile, sandbox compatibility under P0.1's inner bwrap, state persistence across calls, latency per call, packaging and distribution
- [x] Write-up: `docs/spikes/extension-mechanism.md`

### P0.4 — Decisions and design freeze — `in progress` 🧑 (ADR-0001 and ADR-0003 accepted and the dev plan bumped to v0.2 on 2026-09-06; ADR-0002 waits on the P0.1 login-node run; the P1 freeze declaration is the human's)

- [x] **ADR-0001** Out-of-process tool mechanism (from P0.3), recording the D8 tier split — accepted 2026-09-06
- [ ] **ADR-0002** Sandbox stack on the HPC login node and fallback (from P0.1)
- [x] **ADR-0003** Provider client shape: one OpenAI-compatible client with quirk flags (from P0.2) — accepted 2026-09-06
- [x] Update the dev plan (§3, §4, §8, §11) with anything the spikes changed and with D1–D20; bump to v0.2 (2026-09-06; §5, §6, §13–§15 refreshed too; the specs are named as the normative surface)
- [ ] 🧑 Freeze the crate list and the `kernel` public surface for P1

### P0.5 — CI stand-in stack — `done` (every piece tested: fake Slurm and mock solver self-tests on every PR, the two containers proven green in the opt-in `standin` job on PR #4)

Per D18 and D9. Everything agents need to run integration tests without GPUs, cluster access, or in-house tools.

- [x] llama.cpp server container with a small tool-calling model, exposed as an OpenAI-compatible endpoint (`ghcr.io/ggml-org/llama.cpp:server-b10818` + Qwen2.5-1.5B-Instruct GGUF in `standin/compose.yaml`; green in the `standin` job on PR #4, run 34017081578)
- [x] LiteLLM proxy container routing `stand-in/<model>` to it (`ghcr.io/berriai/litellm:v1.99.1` + `standin/litellm/litellm_config.yaml`; same run)
- [x] Fake `sbatch` / `squeue` / `scancel` scripts: run the job in the background, write a Slurm-like log, call the epilog hook on exit (`standin/slurm/`, 55 self-test assertions)
- [x] Mock in-house solver: a script at a fixed "install path" that consumes an input deck, sleeps, and emits a plausible log with convergence lines, timings, and a controllable failure mode (`standin/solver/`, 33 self-test assertions)
- [x] Docker/Podman compose file bringing all four up; **opt-in** CI job (label `ci:standin` or manual, D18 as amended); proven green on PR #2 with the P0.2 client as its first consumer. The P1.5 integration tests plug into the marked hook step when they exist.
- [x] Environment-gated test tier for real vLLM, real LiteLLM upstreams, and real Slurm, skipped in CI (convention in `docs/standin.md`, `standin/env-gate.sh`; no Rust tests use it yet)

### Exit criteria — Phase 0

- [ ] Sandbox spike green on the HPC login node under the site uid map, or a fallback chosen and documented in ADR-0002
- [ ] Provider spike green against vLLM, LiteLLM, and the llama.cpp stand-in, including tool calling and the reasoning field
- [x] Out-of-process tool mechanism chosen and written up as ADR-0001 (accepted 2026-09-06)
- [x] CI stand-in stack runs in CI (`standin-scripts` on every PR; the opt-in `standin` job green on PR #2 and PR #4)
- [ ] All three spike write-ups merged under `docs/spikes/`

---

## Phase 1 — Kernel + local daemon

**Purpose:** the core (§4) plus enough around it to run a real session locally and replay it. Soft freeze of `kernel` at exit (D12).

### P1.0 — Interface specs — `done` (all three approved at v0.1 on 2026-09-06)

Per D20. Agents implement against signatures, not prose. Each spec is reviewed by a human before P1.1 starts.

- [x] (draft v0.1) `docs/specs/kernel-interface.md`: Rust signatures for `State`, `Tool`, `ToolKind`, `ToolResult`, `Task`, `Capability`, `Middleware`, `Provider`, `Host`, `ArtifactStore`, `Memory`, `SandboxBackend`; the task state machine (D1); the session state machine (D2); cancellation, retry, and crash-recovery semantics (D15)
- [x] (draft v0.1) `docs/specs/event-schema.md`: envelope, every `Event` kind and its payload fields, what each hash covers, the volatile-field list excluded from the state hash (D3, D13)
- [x] (draft v0.1) `docs/specs/profile-schema.md`: TOML schema for model profile, agent profile, project overrides, bundles file; merge rules; middleware priority slots; system prompt block order; validator rejection cases; capability atoms and the narrower-than relation (D6, D7)
- [x] 🧑 All three reviewed and approved (2026-09-06, v0.1 as on `main`)

### P1.1 — Core types — `done`

- [x] `State` (serde) with `schema_version` from the first commit; fields: messages, `pending_tasks`, `session_status` (D2), active profile hashes, memory pointer, notebook path, sandbox policy hash, sandbox backend name (D14)
- [x] `State` migration hook: loading an older `schema_version` runs a registered migration or fails loudly
- [x] Content blocks: text, thinking, tool use, tool result, and `Image{artifact_handle, mime}` (D15)
- [x] `Tool` trait: `name`, `description`, `schema`, `kind: Stateless | Session` (D5), `capabilities: Vec<Capability>`, async `invoke() -> Result<ToolResult>` (D4)
- [x] `ToolResult = Value | Task { id, status, eta, check_hint }`; `Task` status enum per D1
- [x] `Capability` atoms and `narrower_than` per D6, with property tests (reflexive, transitive, path-prefix and allowlist cases)
- [x] `Middleware` trait (async): `before_model`, `after_model`, `before_tool`, `after_tool`, `on_compact`, `on_resume`
- [x] `ArtifactStore` and `Memory` traits with no-op implementations (D12)
- [x] `Event` enum covering every kind in §4.2 plus `TaskUpdate`, `Cancelled`, `ProfileLoad`; tool and model events carry the D13 fields
- [x] Hashing module: BLAKE3 over RFC 8785 canonical JSON, prefixed hash strings, `request_hash` and `state_hash` definitions (D3)
- [x] Unit tests for serde round-trips and hash stability of every core type (`crates/kernel/tests/core_types.rs`; RFC 8785 vectors in `hash`; `Capability` property tests in `capability`)

### P1.2 — The loop — `done`

- [x] Implement the §4.1 loop with async hooks in the stated order, checkpoint after each turn, and the D1 suspension rule
- [x] Session state machine per D2, including queued user input at turn boundaries
- [x] Cancellation token checked between hooks; running tool receives SIGTERM via the sandbox launcher; `Cancelled` event logged (D15)
- [x] Provider retry with backoff on rate limits and server errors; on exhaustion the turn fails and the session enters `failed` with checkpoint intact (D15)
- [x] Result spill in the kernel: results over the profile's cap go to `ArtifactStore` and are replaced by `{ handle, head, tail }` (D12)
- [x] Ordered middleware chain; the *resolved* chain is written to the event log at run start
- [x] Tool registry: the loop can only invoke tools registered at construction (enforcement level 1 of §6)
- [x] Loop tests with a fake provider and fake tools: plain turn, tool call turn, multi-tool turn, task started then suspend, task completion injected while running, cancellation mid-tool, retry exhaustion, spill (`crates/kernel/tests/loop_{turns,tasks,cancel,retry,spill,middleware,open,registry}.rs`, 60 tests; the five §6 invariants each have an assertion helper)

### P1.3 — Event log and checkpoints — `done`

- [x] Append-only JSONL writer with the D3 envelope; one file per session; never rewritten
- [x] Redactor in the log writer: known secret values and common token patterns scrubbed before any payload is written (D10)
- [x] Checkpoint = event carrying the state hash plus enough to restore `State`
- [x] `restore(checkpoint_hash) -> State`
- [x] Crash recovery: on start with an existing log, restore the last checkpoint and discard later events (D15) (`Kernel::open`; `crates/kernel/tests/integration_log.rs::crash_mid_turn_recovers_from_the_last_checkpoint_and_discards_the_tail` on a real file)
- [x] Suspend persists state and releases the process; resume restores from the latest checkpoint and continues (`Kernel::open` with a `TaskUpdate` cause on the reopened file)
- [x] Test: run → suspend → resume produces the same payload sequence as an uninterrupted run (`integration_log.rs::run_suspend_resume_in_a_new_process_matches_an_uninterrupted_run`: identical final `state_hash`, messages, tasks, and hashed-event projection)
- [x] Test: a secret value placed in a tool result never appears in the log file (`crates/kernel/tests/log_file.rs::d10_registered_secret_in_a_tool_result_never_reaches_the_file`; pattern corpus in `redact_corpus.rs`)

### P1.4 — Record/replay — `done`

- [x] Recorder middleware captures model responses and tool results keyed by `(checkpoint_hash, request_hash)`
- [x] Replay provider and replay tool-invoker serve recorded responses; a cache miss is an error, not a live call
- [x] `diff-logs` command: strips envelope fields and asserts byte-identical payloads (D16)
- [x] Test: a recorded session replays in under a second with zero network access and `diff-logs` passes (`crates/kernel/tests/replay_sessions.rs::a_recorded_session_replays_in_under_a_second`: 13 ms debug; `ReplayProvider` holds no client; 23 record/replay scenarios assert `Recorder::cassette() == Cassette::from_log`; `replay_diff.rs` covers every §5.3 volatile field and the `diff-logs` binary)

### P1.5 — `providers` crate — `done` (stand-in integration tests run locally against a native stand-in and, on 2026-09-06, green in the opt-in `standin` CI job against the pinned images for the first time — PR #4, run 34017081578)

Depends on P0.2 / ADR-0003.

- [x] One OpenAI-compatible client with SSE streaming
- [x] Per-endpoint quirk flags from the P0.2 table (tool format, reasoning field name, structured-output support, auth mode)
- [x] Reasoning/thinking mapped into a `Thinking` content block that survives serialization
- [x] Image content blocks encoded from artifact bytes at request time (D15)
- [x] Secrets resolved from Host handles at request time only (D10)
- [x] Token usage from every response recorded into the model call event (D13)
- [x] Integration tests against the P0.5 stand-in in CI (`crates/providers/tests/standin.rs`, feature `standin-integration`, run by the opt-in `standin` job with `GRIST_REQUIRE_STANDIN=1`); vLLM and LiteLLM tests behind the environment gate (skip when unset). **Verified locally 2026-09-06**: all five pass under `GRIST_REQUIRE_STANDIN=1` (three stand-in tests exercised, two gated tiers skip) against a native stand-in — llama.cpp b9290 + LiteLLM 1.99.0, no container engine on that host, versions and quirk rows in `docs/spikes/providers.md` §2.4; the same section records `reasoning_content` → `Thinking` end to end against Qwen3-1.7B. **First `ci:standin` run, 2026-09-06** (PR #4, run 34017081578, step `Provider integration tests (P1.5)`): the same five tests green against the pinned `server-b10818` + LiteLLM `v1.99.1` images, which is also the first CI exercise of the 400 `No connected db` case in `standin_litellm_rejects_a_bad_key_as_auth`.

### P1.6 — `host` crate — `done`

- [x] `Host` trait: filesystem, process spawn, network, secrets-as-handles (D10), `ask_user` (D17)
- [x] `host::native` implementation (`NativeHost`, also the `SecretResolver`; `AskUserTool`; `ChannelPrompter`/`NoUserPrompter`)
- [x] Filesystem operations take a `Policy` and enforce path and mode checks in process; this is how `read`, `write`, `edit` are sandboxed (D5)
- [x] `host::remote-client` left as a stub with a documented interface (filled in P3)
- [x] Kernel and tools take `&dyn Host`; nothing in `kernel` touches `std::fs` or `std::process` directly (the kernel's only file I/O is the event log writer; `crates/kernel/tests/no_std_fs_process.rs` asserts it)

### P1.7 — `sandbox` crate and base tools — `done` (the five `bwrap` tests pass on a real bwrap host as of 2026-09-06: a Debian bookworm container under a rootless Podman machine, bwrap 0.8.0, kernel 7.1.8. That host is not the HPC login node, so P0.1 and ADR-0002 are untouched; if the login-node run picks a different inner shape, the `policy_to_args` mapping is what changes)

Depends on P0.1 / ADR-0002 for the *final* inner shape; the mapping is one pure function and is re-verified by the same five tests on whatever host ADR-0002 names.

- [x] `SandboxBackend` trait with `Bwrap` and `None` implementations; `None` compiles only under a `dev-sandbox-none` feature and is logged in every run (D14)
- [x] Inner policy type: mounts (RW/RO), tmpfs scratch, network on/off + allowlist, timeout, scrubbed environment (D10)
- [x] `derive_policy(tool.capabilities, profile.grants) -> Policy`, purely mechanical, no escape hatch
- [x] Stateless launcher: one bwrap per call. Session launcher: one bwrap process per session, calls as RPC (D5)
- [x] Base tools: `read`, `write`, `edit` (in-process via Host policy checks), `bash` (Stateless, bwrap)
- [x] `run_script` tool returning a `Task`, plus the in-kernel process-exit waker that completes it (D1)
- [x] Python REPL as a `Session` tool per ADR-0001, inside the inner sandbox
- [x] Tests: `fs.ro` tool cannot write (`tools::write_under_a_read_only_policy_is_denied`, and `bwrap::fs_ro_mount_refuses_a_write_and_rw_allows_it`, run on every PR by the CI `test` job under `GRIST_REQUIRE_BWRAP=1`); tool without `net` cannot open a socket (`bwrap::network_off_cannot_open_a_socket`, same); timeout enforced (`none_backend::timeout_is_enforced_and_the_process_is_gone`, `session::per_call_timeout_kills_the_process`); tool env contains no API key (`none_backend::none_env_scrubbed`, `policy_args::env_is_scrubbed_to_the_allowlist_with_home_forced`); `run_script` future yields the exit outcome without polling (`tools::sandboxed::run_script_returns_a_task_and_the_future_yields_the_exit_outcome`); the kernel-level suspend/resume cycle completes without polling in context (`crates/orchestrator/tests/exit_criteria.rs::run_script_session_suspends_and_the_process_exit_waker_resumes_it`)

### P1.8 — `profiles` crate and catalog — `done`

- [x] TOML schemas per `docs/specs/profile-schema.md` (D7): model profile (system prompt variant, tool-description phrasings, `tool_format`, thinking/temperature defaults, context length, compaction thresholds, quirks, optional `trained_against_profile_hash`), agent profile (capability bundles, MCP servers, skills, `AGENTS.md`, sub-agent definitions, middleware entries with priority, memory module, sandbox policy, eval set pointer, `context_budget_tokens` default 40000 (D16)), project overrides
- [x] Bundles file mapping `meshing`, `solver`, `post`, and HPC in-house tool grants (`Fs{<install path>, ro}` + `Proc{sbatch}`) to atoms (D6, D9)
- [x] Resolution: `kernel defaults + model profile + agent profile + project overrides` with the D7 merge rules and priority-sorted middleware chain
- [x] System prompt assembly in the D7 block order
- [x] **Validator:** unknown keys, kernel-only keys, and capabilities exceeding grants are rejected; tests for each (`crates/profiles/tests/validator.rs`: all 33 §9 cases and sub-cases, plus the four `warns_*`)
- [x] Every loaded profile is content-hashed; hashes recorded in `State` and in a `ProfileLoad` event
- [x] Catalog: registry of task agents = (model profile, agent profile) pairs, addressable by name
- [x] One default agent (four base tools + `run_script` + Python REPL) shipped in-repo (`profiles/`; `tests/shipped.rs` resolves it end to end)

### P1.9 — Protocol server and first client — `not started`

Open question §15.2 (wire protocol) is decided here.

- [x] Evaluate Agent Client Protocol (ACP) as the wire format; **ADR-0004** records adopt / extend / own-schema-with-ACP-shim — **accepted 2026-09-06** (`docs/adr/0004-wire-protocol.md`): adopt ACP v2 on both legs, carry the twelve log-facing event kinds and the `Tool`/`Task` cancel scopes in a `_grist/*` namespace, unix-socket transport plus a `grist-connect` stdio forwarder, Zed as the blessed first client (JetBrains second), Tauri deferred to P3, `_grist/event` opt-in per session with the socket as the security boundary
- [ ] JSON-RPC over stdio (kernel) and unix socket (supervisor daemon that spawns one kernel process per session, D4)
- [ ] Socket auth: file permissions plus peer-credential check (D11)
- [ ] Protocol covers: start session, send user message, stream events, `ask_user` round trip (D17), cancel (D15), suspend, resume, replay, end session
- [ ] Remote use documented as an SSH-forwarded unix socket; no websocket in this phase (D11)
- [ ] First client: **Zed** as an ACP client over `grist-connect` (ADR-0004 §2; JetBrains second known-good), configured by a checked-in `agent_servers` snippet; runs on Windows, macOS, and Linux (D14). No CLI (§13, "CLI-less"); the Tauri shell moves to P3
- [ ] Every protocol message maps to or from an `Event`; no protocol-only state
- [ ] Launcher: `profiles::KernelInputs` → `KernelConfig` + `SessionInit` (reference shape in `crates/orchestrator/tests/launcher.rs`), backend selection by `sandbox_backend` (refuse `none` outside dev builds), endpoint URL via `host::endpoint_url_from_env`, provider from `Quirks`, model-profile `tool_descriptions` applied by wrapping the compiled tools' definitions

### Exit criteria — Phase 1

- [x] A coding session (read/edit/bash on a real repo) is recorded and replays with `diff-logs` passing and no network (`crates/orchestrator/tests/exit_criteria.rs::coding_session_replays_with_diff_logs_passing_and_no_network`: real checkout, `NativeHost`, `None` backend; replay never calls the provider, leaves the checkout untouched, and `diff_logs` is identical)
- [x] A `run_script` session (start, suspend, process-exit waker, resume, finish) is recorded and replays the same way (`exit_criteria.rs::run_script_session_replays_with_diff_logs_passing_and_no_network`)
- [x] Session reaches `failed` on provider exhaustion and resumes from its checkpoint (`crates/orchestrator/tests/exit_criteria.rs::provider_exhaustion_fails_the_session_and_it_resumes_from_its_checkpoint`, in-process and from the file)
- [ ] 🧑 `kernel` soft-frozen: changes now require an ADR (D12) — declared by the human at Phase 1 exit, after P1.9; the CODEOWNERS/CI check in the boundary track lands with it
- [x] `State.schema_version` migrations exercised by at least one test (`crates/kernel/tests/core_types.rs::migration_registry_runs_registered_steps_and_fails_loudly_otherwise`, `loop_open.rs::open_migrates_an_old_checkpoint_and_logs_state_migrated`, `log_recovery.rs::an_older_schema_checkpoint_restores_through_a_registered_migration`)

---

## Phase 2 — Extensions and specialization

**Purpose:** everything that makes an agent *specific*, built on the extension API third parties will use (§1 goal 2). Context discipline defaults from §7 land here. Hard freeze of `kernel` at exit (D12).

### P2.1 — Extension API — `not started`

Depends on ADR-0001 and D8.

- [ ] Compiled tier: `ext` API for Rust middleware and first-party tools
- [ ] Out-of-process tier: manifest (name, version, tools provided, capabilities required) and loader for the ADR-0001 mechanism; these run under the Session or Stateless sandbox launcher
- [ ] A profile cannot load an extension that requests capabilities beyond the profile's grants
- [ ] Example third-party out-of-process tool in `examples/`

### P2.2 — MCP client with lazy exposure — `not started`

- [ ] MCP client (stdio and HTTP transports) as an `ext` module; each server is a `Session`-kind tool host (D5)
- [ ] Servers register at session start but their tool schemas are **not** put in the prompt
- [ ] `find_tools(query)` tool surfaces relevant MCP tools for the next turn only
- [ ] Skills can reference MCP tools by name, which exposes them for that turn
- [ ] MCP tool calls run under the policy derived from the server's declared capabilities
- [ ] Test: prompt token count with N registered servers is independent of N

### P2.3 — Skills loader — `not started`

- [ ] Skill = markdown with frontmatter (name, description, tools referenced, capabilities required)
- [ ] Loaded from paths named in the agent profile; content-hashed and logged as a `ProfileLoad` event
- [ ] Skill invocation injects the skill body into the skills block of the system prompt for the turn (D7)

### P2.4 — Sub-agent spawn with monotone narrowing — `not started`

- [ ] Sub-agent definitions as markdown-with-frontmatter (from dcode, §2), referencing a catalog entry
- [ ] `spawn(catalog_name, task)` tool returning a `Task` handle; the child is a separate kernel process (D4)
- [ ] Enforcement level 1: child kernel is constructed with only the child's allowlisted tools
- [ ] Enforcement level 2: child inner sandbox policy derived from that same list
- [ ] Enforcement level 3: every child atom is `narrower_than` some parent atom, else spawn is rejected (D6)
- [ ] `request_capability` message from child to parent (the child asks; it never acquires)
- [ ] `spawn` and child completion recorded as events in the parent log with a link to the child log

### P2.5 — Artifact store — `not started`

Spill itself already lives in the kernel (D12); this milestone provides the real store and the domain handlers.

- [ ] Content-addressed `ArtifactStore` implementation (BLAKE3 hash → bytes) with a local filesystem backend
- [ ] `read_artifact(handle, range)` tool
- [ ] Solver-log structured-extraction pass (errors, convergence, timings) as a spill handler, developed against the P0.5 mock solver log and kept behind an interface so the in-house solver's format can be plugged in later (D9)
- [ ] Visualization results returned as `Image` artifacts, not inline data

### P2.6 — Notebook-based compaction middleware — `not started`

- [ ] Lab-notebook file path in `State`; harness re-injects it as the notebook block in `on_resume` (D7)
- [ ] `on_compact` summarizes *toward the notebook* (updates the notebook, then trims context), not into a lossy paragraph
- [ ] Compaction thresholds read from the model profile's context length
- [ ] Compaction recorded as an event with before/after hashes
- [ ] Test: replay a long session with and without compaction; notebook content is preserved

### P2.7 — Memory module interface — `not started`

Open question §15.3 is decided here (**ADR-0005**: how much of ALMA's search space is exposed).

- [ ] `Memory` trait (defined in P1.1) finalized: `store`, `retrieve`, `update`, `compress`, `forget`
- [ ] Memory pointer in `State`; memory snapshots content-addressed
- [ ] One baseline implementation (file-backed notes with keyword retrieval)
- [ ] Memory module selected by agent profile; the module is data the evolve loop may replace

### P2.8 — First task agents — `not started`

In-house tools are placeholders per D9; profiles reference the bundles file, not concrete binaries.

- [ ] Orchestrator profile: broad capabilities, may spawn, owns the notebook
- [ ] "Model debugger" profile: scoped to `solver` + `post` + `fs.rw:workdir`; **no** `meshing`
- [ ] Both registered in the catalog with model profiles for at least one vLLM-served and one LiteLLM-routed model, plus the llama.cpp stand-in for CI

### P2.9 — Context budget instrumentation — `not started`

- [ ] Per-turn context token count emitted as an event
- [ ] `context_budget_tokens` from the agent profile (default 40000, D16); exceeding it is a warning event
- [ ] Measurement workload: a mock pipeline (mesh → solve → post) driven through the P0.5 fake Slurm and mock solver, standing in for the in-house pipeline until it exists

### Exit criteria — Phase 2

- [ ] A test proves the model debugger sub-agent cannot invoke meshing tools at all three enforcement levels
- [ ] Context per turn stays under the profile budget across the full mock pipeline, measured via P2.9
- [ ] All first-party extensions are built on the P2.1 API and nothing else
- [ ] ADR-0005 (memory interface scope) merged
- [ ] `kernel` hard-frozen (D12)

---

## Phase 3 — Provenance and orchestration

**Purpose:** make the agent traceable like any other data-producing process (§10) and runnable near the data (§11).

### P3.1 — Provenance projector — `not started`

- [ ] W3C PROV-DM schema in SQLite: `Entity`, `Activity`, `Agent`; edges `used`, `generated`, `wasDerivedFrom`, `wasAttributedTo`, `wasInformedBy`
- [ ] Agent identity = `(model_id, model_profile_hash, agent_profile_hash, kernel_version)`
- [ ] Projector folds the JSONL event log into the schema using the D13 fields; idempotent; re-runnable from scratch
- [ ] The agent has no write path to the graph; annotations are entities `wasAttributedTo` the agent
- [ ] Profile changes produce a *new* agent node with a `wasDerivedFrom` edge (§10 rule 2)
- [ ] Postgres backend behind the same interface (open question §15.4 → **ADR-0006**)

### P3.2 — Artifact metadata and "why this artifact" queries — `not started`

- [ ] Artifact store entries projected as `Entity` rows with hash, size, type, producing activity
- [ ] Query: artifact hash → producing tool call → checkpoint → reasoning trace, skill, memory snapshot
- [ ] Query: per-profile tool usage statistics, sub-agent request patterns, eval outcomes (inputs to P4)
- [ ] Exposed over the protocol so the UI can show a provenance view

### P3.3 — Orchestrator: placement — `not started` 🧑

Per D4, D11, D18. The P1.9 supervisor grows into this crate.

- [ ] `orchestrator` crate: launch a kernel process with a placement of `local` or `runner`
- [ ] `runner` = HPC login node reached over SSH; the kernel runs in rootless Podman there with the site uid map (D18); the client connects to an SSH-forwarded unix socket (D11)
- [ ] Outer bwrap applied by the launcher, outside the mutable layer; the harness cannot see or alter it
- [ ] `host::remote-client` (from P1.6 stub) so a remote kernel's host calls reach the right filesystem
- [ ] 🧑 Verify the full path from a laptop client to a login-node kernel
- [ ] `podman` placement on other Linux hosts and OpenShift placement are in the Backlog

### P3.4 — Wakers and trust tiers — `not started` 🧑

- [ ] Waker interface: "deliver `TaskUpdate` for task X" (D1)
- [ ] Wakers: Slurm epilog hook, file watcher, cron / self-schedule, inbound webhook, polling sidecar (uses `check_hint`)
- [ ] Trust tier per trigger source (interactive > scheduled > inbound webhook) selects the outer sandbox tier
- [ ] Resume event records the waker source and tier
- [ ] Slurm wrapper tool for in-house solvers at pre-installed paths: submits via `sbatch`, returns a `Task`, completed by the epilog waker (D9)
- [ ] Test in CI: the fake `sbatch` and epilog from P0.5 complete a suspend/resume cycle with no polling in context
- [ ] 🧑 Same test against real Slurm on the login node

### P3.5 — Agent-to-agent messaging — `not started`

- [ ] Messaging over the same protocol as UI ↔ kernel
- [ ] External agentic systems wrapped as tools that return `Task` handles
- [ ] Inbound messages carry a trust tier like any other trigger

### P3.6 — Workflow runner — `not started`

- [ ] Workflow schema in TOML: nodes are `{ task agent | tool | pure function | sub-workflow }`; edges are static or routing functions
- [ ] Small Rust builder for cases needing real code
- [ ] Runner executes a workflow, checkpointing per node; each node run is an `Activity` in provenance
- [ ] Reference pipeline: mesh → setup → solve → post → viz, with in-house tool nodes as placeholders and the P0.5 mocks in CI (D9)
- [ ] Guardrail documented and enforced in review: a node that needs loops-with-state becomes an agent profile, not a workflow feature (§9)
- [ ] Checkpoint forking: "replay run N from turn M with a different middleware chain" as a runner operation

### P3.7 — Fleet basics — `not started`

- [ ] List / inspect / suspend / resume running agents across placements
- [ ] Session logs and artifacts from remote placements retrievable locally

### Exit criteria — Phase 3

- [ ] The reference pipeline completes in CI against the mocks with each product traceable to the checkpoint and profile version that produced it, via P3.2 queries
- [ ] 🧑 The same pipeline runs on the login node, submitting real Slurm jobs, resumed by the real epilog waker
- [ ] Agent has no write path into the provenance graph (test asserts the projector ignores agent-authored pedigree)
- [ ] ADR-0006 (provenance store at fleet scale) merged

---

## Phase 4 — Evolve loop

**Purpose:** make the harness a searchable object with promotion gates that reject overfitting (§12). The outer loop stays deliberately dumb; the proposer has full raw traces.

### P4.1 — Archive exposure — `not started`

- [ ] Filesystem view over event logs + artifact store + provenance DB that a proposer agent can `grep`
- [ ] Read-only mount into the proposer's inner sandbox
- [ ] Index of prior candidates, their diffs, their scores

### P4.2 — Eval runner — `not started` 🧑

- [ ] Eval set format: task, inputs, process-based judge (correct setup, convergence, sane post-processing, tolerances), never exact numbers for simulation tasks (§12.6)
- [ ] Each profile points at a visible dev set and a **hidden** held-out set
- [ ] Hidden sets live in a separate private repository mounted only into the eval runner's sandbox; a test asserts the proposer's policy cannot mount it (D19)
- [ ] 🧑 Named owner of the hidden half recorded here: ______
- [ ] Parallel-sampling baseline: same model, same token budget, N samples, best-of-N by the same judge
- [ ] Reports success *and* token efficiency side by side, using the D13 usage fields
- [ ] Runs candidates via record/replay first (P1.4) and checkpoint forking (P3.6); live runs are one Podman container per candidate
- [ ] 🧑 First simulation eval set authored against the in-house tools (D9)

### P4.3 — Promotion gates — `not started`

- [ ] Gate 1: held-out gain over the current profile
- [ ] Gate 2: no regression on any *other* profile's eval set
- [ ] Gate 3: beats the parallel-sampling baseline at equal token cost
- [ ] Gate 4: diff reviewed (human or reviewer-agent) and the review is provenance-linked
- [ ] Gate 5: inner sandbox policy changes pass a static check (atoms may only narrow; never touches the outer sandbox)
- [ ] Gate results stored as provenance entities on the candidate agent node

### P4.4 — Proposer profile — `not started`

- [ ] Agent profile with archive access (P4.1), no network, no eval-set write access, no hidden-set mount (D19)
- [ ] Search space limited by allowlist to the mutable layer: prompts, skills, tool descriptions, middleware chains, workflows, memory modules, inner sandbox policy, tool allowlists, out-of-process tools (D8). Kernel, compiled middleware, and outer sandbox are unreachable by construction.
- [ ] Output = a candidate profile diff + a written hypothesis naming the concrete failure mode it fixes

### P4.5 — Evolve workflow — `not started`

- [ ] The outer loop itself is a P3.6 workflow: propose → replay-eval → live-eval → gates → promote or reject
- [ ] Promotion creates a new catalog entry with `wasDerivedFrom` the previous one; the old one is never deleted
- [ ] Rejection is recorded with which gate failed

### P4.6 — First targets — `not started`

- [ ] Memory module search (ALMA-style) over P2.7 implementations
- [ ] Tool-allowlist pruning per profile, driven by P3.2 usage statistics; open question §15.5 → **ADR-0007**
- [ ] Compaction thresholds per model profile

### P4.7 — Training-data closure — `not started`

- [ ] Export successful, profile-labelled traces in a fine-tuning-ready format (feeds P5.3)

### Exit criteria — Phase 4

- [ ] One change promoted with a held-out gain and a complete provenance trail from candidate to catalog entry
- [ ] One deliberately constructed overfit change (D16) is rejected by gate 1 with the rejection recorded
- [ ] The proposer demonstrably cannot read the held-out set or modify `kernel` / outer sandbox (tests)
- [ ] ADR-0007 merged

---

## Phase 5 — Fine-tuned specialists

**Purpose:** harness evolution and weight training share one source; the harness must not silently drift out from under a fine-tune (§14).

### P5.1 — Fine-tune model profile — `not started` 🧑

- [ ] Model profile for a vLLM-served fine-tune with `tool_format: parsed(<syntax>)` and `trained_against_profile_hash`
- [ ] Drift warning: loading a fine-tune whose trained-against hash does not match the current resolved profile emits a warning event and is visible in the UI
- [ ] Parser middleware for the fine-tune's tool-call syntax

### P5.2 — Specialist eval set — `not started` 🧑

- [ ] Eval set (dev + hidden, D19) specific to the specialist's role, using the P4.2 format
- [ ] Specialist registered in the catalog as a task agent; spawnable by the orchestrator profile like any other

### P5.3 — Trace export pipeline — `not started`

- [ ] P4.7 export productionized: filter by profile, outcome, and eval score; redaction re-applied (D10); content-hash the export as a provenance entity
- [ ] Round trip documented: traces → fine-tune → new model profile → drift check

### Exit criteria — Phase 5

- [ ] A fine-tuned specialist runs under its own profile, passes its eval set, and its drift warning fires when the profile is changed
- [ ] A training-data export is produced from real traces and is provenance-linked

---

## Backlog (deliberately deferred)

Not scheduled. Each needs a human decision or an external dependency before it can be planned.

| Item | Why deferred | Unblocks when |
|---|---|---|
| OpenShift placement for the kernel | Security context constraints differ from the login node; access patterns need the security team (D11, D18) | Security team engagement |
| Websocket transport with bearer-token auth and TLS | Remote access is SSH-forwarded for now (D11) | OpenShift decision, or a browser UI requirement |
| `podman` placement on arbitrary Linux hosts | Only the login node and local are needed now (D18) | A second deployment target |
| Native macOS sandbox backend | macOS is dev-only; Podman machine or `None` suffices (D14) | Never, unless deployment changes |
| Middleware as an out-of-process extension | Rejected in D8 | An ADR overturning D8 |
| Native Anthropic / OpenAI provider clients | §5: later, for prompt caching and provider-specific features | After P2 |
| Sub-agents and tooling around the in-house solvers | Tools are placeholders (D9) | In-house tool interfaces documented |

---

## Cross-cutting tracks

Checked at every phase boundary.

### Kernel boundary discipline

- [x] `CONTRIBUTING.md` rule in place (P0.0)
- [ ] CI check or CODEOWNERS on `crates/kernel/` requiring an ADR link in the PR (from P1 exit onward)
- [x] `kernel` has no dependency on any other in-repo crate (enforced by the P0.0 DAG and a test: `crates/kernel/tests/no_in_repo_deps.rs`)

### Schema versioning

- [ ] `State.schema_version` (P1.1: `MigrationRegistry` + a v0→v1 migration test in `core_types.rs`), event schema version, profile schema version, PROV schema version each bumped with a migration and a test

### Documentation

- [x] `docs/adr/` index current (ADR-0001 and ADR-0003 accepted; ADR-0002 and ADR-0004 pending)
- [x] `docs/specs/` kept in step with the code; a spec change and its implementation land in the same PR (every P1.x clarification is marked **[clarified in P1.x]** in the spec that owns it)
- [x] Each crate has a `README.md` stating its responsibility and whether the evolve loop may mutate it (§3.1 table)
- [ ] Dev plan revised at each phase exit (v0.2 after P0 — done; v0.3 after P1, …)

---

## Risk register tracking

Maps §14 risks, plus two surfaced in review, to the milestones that mitigate them. A risk is *retired* when its mitigating milestones are done and the property is tested.

| Risk | Mitigating milestones | Status |
|---|---|---|
| Extension mechanism chosen wrong | D8, P0.3, P0.4 (ADR-0001), P2.1 | open |
| bwrap won't nest in rootless Podman under the login node's 2002-uid map | P0.1, P0.4 (ADR-0002), P1.7 | open (P1.7's inner shape is verified under bwrap on 2026-09-06, and the nested spike chain passed on a local rootless Podman machine only with `VARIANT=label-disable,unmask`; the site's uid map, kernel and SELinux policy are still untested) |
| In-house tools unavailable to agents and CI | D9, P0.5 mocks, spill handler behind an interface (P2.5) | open |
| Kernel feature creep | P0.0 (CONTRIBUTING), P1.0 specs, D12 freeze schedule, boundary track | open (specs and boundary tests in place; the soft freeze is declared at P1 exit) |
| Profiles overriding structure | P1.8 validator (D7) | mitigated (kernel-only keys, layer-3 forbidden keys, and widening all rejected with tests) |
| Checkpoint schema drift | P1.1 `schema_version` + migration hook, schema versioning track | mitigated (P1.1 hook + test); retire at P1 exit |
| Secrets in the archive | D10: P1.3 redactor, P1.6 handles, P1.7 scrubbed env | mitigated (all three landed with tests: secret never in the file, handles never expose values, sandbox env scrubbed) |
| Evolve loop overfits / optimizes noise | P4.2 hidden sets (D19) + baseline, P4.3 gates, P4 exit constructed rejection (D16) | open |
| Fine-tune / harness drift | P5.1 trained-against hash + warning | open |
| Middleware ordering bugs | P1.2 resolved chain logged per run; D7 priority slots | mitigated (chain logged and same-order hooks tested in P1.2; validator ranges tested in P1.8) |
| Workflow DSL creep | P3.6 guardrail | open |
| Context flooding from MCP | P2.2 lazy exposure, kernel spill (D12), P2.6 compaction, P2.9 budget | open |
| Agent writes its own pedigree | P3.1 mechanical emission, P3 exit test | open |

---

## Decision log (ADR index)

| ADR | Decision | Decided in | Status |
|---|---|---|---|
| ADR-0001 | Out-of-process tool mechanism (process JSON-RPC vs. WASM), recording the D8 tier split | P0.4 | accepted (2026-09-06) |
| ADR-0002 | Sandbox stack on the HPC login node and fallback | P0.4 | pending |
| ADR-0003 | One OpenAI-compatible provider client with quirk flags | P0.4 | accepted (2026-09-06) |
| ADR-0004 | Wire protocol: adopt ACP v2 on both legs, with a `_grist/*` extension namespace, a unix-socket transport plus a stdio forwarder, and an off-the-shelf ACP client | P1.9 | accepted (2026-09-06) |
| ADR-0005 | Memory module interface: how much of ALMA's search space is exposed | P2.7 | pending |
| ADR-0006 | Provenance store at fleet scale: Postgres alone or graph layer | P3.1 | pending |
| ADR-0007 | Sub-agent capability pruning: automated vs. proposed-for-review | P4.6 | pending |

Decisions D1–D20 in `design-decisions.md` predate the ADR process and are binding without one.

## Open questions tracker

From §15 of the dev plan.

| # | Question | Resolves in | Status |
|---|---|---|---|
| 1 | Extension mechanism | D8 narrowed it; P0.3 spike recommends process JSON-RPC; ADR-0001 accepted | decided |
| 2 | Wire protocol (ACP vs. own) | ADR-0004 accepted 2026-09-06: adopt ACP v2, `_grist/*` for the log-facing events and the `Tool`/`Task` cancel scopes, Zed as the first client; transport and auth were already D11 | decided |
| 3 | Memory module interface scope | P2.7 → ADR-0005 | open |
| 4 | Provenance store at fleet scale | P3.1 → ADR-0006 | open |
| 5 | Capability pruning automation | P4.6 → ADR-0007 | open |
| 6 | First simulation eval set and hidden-half ownership | P4.2; storage settled by D19; owner to be named | partly decided |

---

## Change log for this plan

| Date | Change |
|---|---|
| 2026-09-06 | Initial plan derived from dev plan v0.1 |
| 2026-09-06 | Adversarial review; twenty decisions recorded in `design-decisions.md` and folded in. Added P0.5 CI stand-in stack, P1.0 interface specs, Backlog section, human-required markers. |
| 2026-09-06 | P0.0 repository foundations landed: workspace, nine stub crates, DAG, CI, toolchain pin (1.94.1, MSRV 1.94), `CONTRIBUTING.md`. |
| 2026-09-06 | P0.3 done (process JSON-RPC recommended, ADR-0001 drafted). P0.2 client + write-up + ADR-0003 draft; real endpoints remain 🧑. P0.5 fake Slurm and mock solver tested; containers and CI job authored, pending first green run. P0.1 spike scripts ready for the login node. P1.0 specs drafted at v0.1 and reconciled, awaiting review. |
| 2026-09-06 | Model-serving stand-in stack made opt-in in CI (label `ci:standin` / manual); fake Slurm and mock solver self-tests stay on every PR. D18 amended accordingly. |
| 2026-09-06 | P1.0 specs approved at v0.1; ADR-0001 and ADR-0003 accepted. P1.1 core types landed in `crates/kernel` (in-crate RFC 8785 canonicalizer with `ryu-js`; `serde_json` `float_roundtrip`; `proptest` dev-dependency). Spec clarifications marked **[clarified in P1.1]**: `Net` port rule and host normalization in `narrower_than`; the sandbox envelope excludes `secret:` atoms from `caps`. |
| 2026-09-06 | P1.5 `providers` landed: one OpenAI-compatible SSE client with `Quirks` from the model profile; 41 unit tests over fake vLLM/LiteLLM/llama.cpp/Hermes shapes; stand-in integration tests behind `standin-integration`, wired into the opt-in CI job. Clarifications recorded in `docs/specs/kernel-interface.md` §3.8 as **[clarified in P1.5]**. |
| 2026-09-06 | P1.6 `host` landed: `NativeHost` (policy-checked filesystem with symlink resolution, scrubbed-env spawn with SIGTERM→SIGKILL, allowlisted network via reqwest, secrets as handles registered with the redactor on resolve), `AskUserTool` + prompters, `remote-client` stub, endpoint-URL helper; 50 tests. Clarifications recorded in `kernel-interface.md` §3.9 as **[clarified in P1.6]**. |
| 2026-09-06 | P1.3 log landed: `FileEventLog` (JSONL, fsync on checkpoints, torn-tail tolerance, seq/session/schema validation), one writer path with the writer-side redaction pass for both logs, restore with hash verification and migrations, log-level recovery tests, the §4.4 redaction corpus and the D10 acceptance test (52 tests). Kernel-level suspend/resume and recovery tests follow P1.2. Clarifications recorded in `event-schema.md` §1 as **[clarified in P1.3]**. |
| 2026-09-06 | P1.7 `sandbox` landed: `BwrapBackend` (pure `policy_to_args` against the P0.1 inner shape), `NoneBackend` behind `dev-sandbox-none` with a CI-asserted absence from default builds, `JsonRpcSession` (newline JSON-RPC 2.0, SIGTERM→SIGKILL), the six base tools incl. `run_script` (Task + in-process waker future) and the Python REPL; 57 tests, 5 need a bwrap host. Clarifications recorded in `kernel-interface.md` §3.12 as **[clarified in P1.7]**. |
| 2026-09-06 | P1.2 loop landed in `crates/kernel/src/loop_/`: `Kernel`, `KernelHandle`, the §6 turn with every cancellation check point, the D1/D2 state machines, retry with full jitter, kernel spill, ingress redaction, chain resolution, registry enforcement, in-process waker, `open` with resume/recovery/migration; 60 loop tests. Clarifications recorded in `kernel-interface.md` §7 as **[clarified in P1.2]**. |
| 2026-09-06 | P1.8 `profiles` landed with the in-repo `profiles/` tree (catalog, bundles, `stand-in` model, `default` agent): per-file validation with exact TOML paths, D7 merge, bundle and placeholder expansion, symbolic `resolved_profile_hash`, narrowing checks, prompt assembly, `KernelInputs` for the launcher; 63 tests. Clarifications recorded in `profile-schema.md` §13 as **[clarified in P1.8]**. |
| 2026-09-06 | Phase 1 exit-criteria sessions added in `crates/orchestrator/tests/exit_criteria.rs` (real kernel + `NativeHost` + `None` backend + base tools + file log): coding session recorded, `run_script` suspend/waker/resume recorded, provider exhaustion → `failed` → resume proven. The `orchestrator → sandbox, host` edges are used as dev-dependencies for these tests. |
| 2026-09-06 | P1.4 record/replay landed in `crates/kernel/src/replay/`: `Cassette` (the log is the cassette), `Recorder` at 990, `ReplayProvider`, `ReplayTool` with the driver's checkpoint tracker, `ReplayDriver`, `diff_logs` and the `diff-logs` binary; 28 tests. Two kernel-shape additions recorded in `kernel-interface.md` §3.6 as **[clarified in P1.4]**: `ToolResult::Replayed(ToolOutput)` and `ToolOutput.in_process_waker`. |
| 2026-09-06 | Phase 1 exit criteria 1, 2, 3 and 5 met by tests in `crates/orchestrator/tests/exit_criteria.rs` and the kernel migration tests; the soft freeze (criterion 4) waits on P1.9 and the human. |
| 2026-09-06 | Launcher-path test over the shipped `profiles/` tree (`crates/orchestrator/tests/launcher.rs`); `orchestrator → profiles` recorded as a dev edge. Risk register: secrets, middleware ordering, and profile-structure risks marked mitigated. README status refreshed. |
| 2026-09-06 | Dev plan bumped to v0.2 (D1–D20, ADR-0001/0003, spike outcomes folded in; specs named normative). PR #3 opened with the `ci:standin` label. |
| 2026-09-06 | P1.5 verified locally against a native stand-in (no container engine on the host): `standin/up.sh --print-env`, smoke (a)+(b), all five `providers` stand-in tests under `GRIST_REQUIRE_STANDIN=1`, and `spikes/provider-client/run-against.sh standin` on Qwen2.5-1.5B and Qwen3-1.7B; rows and versions in `docs/spikes/providers.md` §2.4, native recipe and two upstream drifts (Qwen3 GGUF filename, LiteLLM `v1.99.1` has no PyPI release) in `docs/standin.md`. P1.7's five `bwrap` tests still unrun: that host is macOS with no `bwrap`, so P1.7 and ADR-0002 are unchanged. Smoke part (c) needs a Linux userland (`setsid`, `flock`, bash ≥ 4) and stays with the `standin-scripts` CI job. |
| 2026-09-06 | **ADR-0004 drafted** (`docs/adr/0004-wire-protocol.md`, status proposed): ACP evaluated against D4, D11, D15, D17, the 28 event kinds and the "Windows/macOS/Linux, no CLI" client requirement. Proposal is to adopt ACP v2 on both legs (unix socket to the supervisor, stdio to the one-session kernel), carry the twelve log-facing event kinds and the `Tool`/`Task` cancel scopes in a `_grist/*` extension namespace, and make the first client an off-the-shelf ACP editor with a stdio↔socket forwarder instead of a bespoke Tauri shell. Not accepted; P1.9 is still `not started`. |
| 2026-09-06 | First green `ci:standin` run of the P1.5 hook step (PR #4, run 34017081578): stack up, smoke all checks passed, P0.2 spike rows, and `GRIST_REQUIRE_STANDIN=1 cargo test -p providers --features standin-integration` → 5 passed against the pinned images. The `standin` job's P1.5 hook is now proven end to end, not just wired. |
| 2026-09-06 | **ADR-0004 accepted**: adopt ACP v2 on both legs (unix socket to the supervisor, stdio to the one-session kernel), `_grist/*` for the twelve log-facing event kinds and the `Tool`/`Task` cancel scopes, `grist-connect` as the stdio↔socket forwarder, Zed as the blessed first client with the Tauri shell deferred to P3, and `_grist/event` as an opt-in per-session subscription whose security boundary is the socket (D11), not a per-kind filter. The maintainer delegated the four open questions; the resolutions are recorded in the ADR. Open question §15.2 is closed; P1.9's first box ticks with this PR.
| 2026-09-06 | **P1.7 done.** The five `bwrap` tests were run for the first time on a real bwrap host: a Debian bookworm container (rust 1.94.1, bubblewrap 0.8.0, Python 3.11.2) under a rootless Podman 6.1.1 machine (Fedora CoreOS, kernel 7.1.8-200.fc44.aarch64, crun 1.29.1, SELinux enforcing; the container needed `--security-opt label=disable --security-opt unmask=ALL` for the inner `--proc` mount). First run: 3 / 5, both failures `bwrap: Can't chdir to /tmp/.tmpXXXX: No such file or directory`, a real bug in `policy_to_args`: the scratch `--tmpfs /tmp` came after the policy mounts and shadowed a mount under `/tmp`. Fixed by emitting the tmpfs before the mounts (`bwrap.rs`, README table, new pure test `policy_args::scratch_tmpfs_precedes_the_policy_mounts_so_a_mount_under_tmp_wins`); second run 5 / 5, and `cargo test --workspace --all-features` in the same container: 384 passed, 0 failed, with clippy and fmt clean. A container inside a Podman machine is a legitimate host for these assertions (read-only mount refuses a write, no-net cannot open a socket, timeout kills the process, env scrubbed, PID 1-ish session survives SIGTERM→SIGKILL) but it is **not** the HPC site: P0.1 and ADR-0002 are unchanged and still need the login node. |
| 2026-09-06 | P0.1 spike scripts rehearsed end to end under the same local Podman machine, purely to debug them; no results recorded in `docs/spikes/sandbox-nesting.md`. Fixed: GNU-only `date -Is`; `VARIANT=keep-ns` was unrunnable (`--userns` and `--uidmap` are mutually exclusive) and now replaces the site map; new composable `unmask` and `label-disable` variants (both were needed on this host: Podman's masked `/proc` paths give EPERM on the inner `--proc`, SELinux gives EACCES first); the inner battery's pid-namespace check counted `/proc` entries from a forking command substitution and now checks `$$`; `results/` and `work/` git-ignored. With `VARIANT=label-disable,unmask` steps 10–40 all PASS locally. Smoke part (c) also run inside the compose `slurm` service via `podman-compose` (all checks passed), closing the one smoke part that cannot run natively on macOS. |
| 2026-09-06 | The five `bwrap` tests now run on every PR: the CI `test` job installs `bubblewrap` (0.9.0) on the ubuntu VM runner, switches off `kernel.apparmor_restrict_unprivileged_userns` (Ubuntu 24.04 ships it on, and under it bwrap fails to configure loopback in its new network namespace: `loopback: Failed RTM_NEWADDR: Operation not permitted`), and sets `GRIST_REQUIRE_BWRAP=1`, which turns their skip into a failure (`crates/sandbox/tests/bwrap.rs`, same convention as `GRIST_REQUIRE_STANDIN`). P1.7's tested properties are continuously asserted rather than verified once by hand. Ticked four boxes that were already true on `main`: P0.5's two containers (green in the `standin` job on PR #4) and the Phase 0 exit criteria for ADR-0001 (accepted) and the stand-in stack running in CI; P0.5 set to `done`. |
