# Workspace crates and the dependency DAG

The crate list follows §3.1 of `docs/agent-harness-dev-plan.md`. The list and the
`kernel` public surface are frozen for Phase 1 at P0.4 (see
`docs/IMPLEMENTATION_PLAN.md`).

## Dependency rules

These rules are binding. `kernel` enforces its own rule with a test
(`crates/kernel/tests/no_in_repo_deps.rs`); CI runs it on every PR.

1. **`kernel` depends on nothing in-repo.** It defines every trait the rest of the
   system implements (`Tool`, `Middleware`, `Provider`, `Host`, `ArtifactStore`,
   `Memory`, `SandboxBackend`) and the core types (`State`, `Event`, `Task`,
   `Capability`). Third-party dependencies are fine; a `path` dependency is not.
2. **Everything else depends on `kernel`** and implements its traits:
   `providers`, `host`, `ext`, `profiles`, `sandbox`, `orchestrator`, `provenance`,
   `evolve`.
3. **Nothing depends on `evolve`.** It is the outer loop and is itself a profile;
   it consumes the others and is consumed by nobody.
4. Cross-edges between the non-kernel crates are allowed only where the plan
   states them. Currently expected:
   - `ext` → `sandbox` (out-of-process tools run under the sandbox launchers, P2.1)
   - `orchestrator` → `sandbox`, `host`, `profiles` (outer bwrap, remote host client, P3.3; the P1.9 launcher assembles a `KernelConfig` from `profiles::KernelInputs`; used as dev-dependencies since P1 for the exit-criteria and launcher integration tests in `crates/orchestrator/tests/`)
   - `evolve` → `provenance`, `profiles`, `orchestrator` (archive, candidates, eval runs, P4)
   - `profiles` → `sandbox` (reserved; P1.8 needed only `kernel::derive_policy_with` and `Capability`, so the edge is not used yet)
   Add a new edge here in the same PR that introduces it.

```
                 ┌────────┐
                 │ kernel │   (depends on nothing in-repo)
                 └───┬────┘
     ┌────────┬──────┼──────┬──────────┬───────────┬────────────┬─────────┐
     ▼        ▼      ▼      ▼          ▼           ▼            ▼         ▼
 providers  host   ext  profiles   sandbox   orchestrator  provenance  evolve
                    │      │          ▲           │                       │
                    └──────┴──────────┘           │                       │
                                                  └───────────────────────┘
                                          (evolve is a sink: nothing depends on it)
```

## Crate table

| Crate | Responsibility | Mutable by evolve loop? | Filled in at |
|---|---|---|---|
| `kernel` | Agent loop, typed `State`, `Tool` trait, `Middleware` trait, event log, checkpoints, suspend/resume | **No** (frozen) | P1.1–P1.4 |
| `providers` | Model clients. v0: one OpenAI-compatible client with per-endpoint quirk flags | No | P1.5 |
| `host` | Trait impls for filesystem, process spawn, network, secrets, UI prompts: `native`, `remote-client` | No | P1.6, P3.3 |
| `ext` | Extension API + first-party extensions: MCP client, sub-agent spawn, skills, memory modules, workflow runner, Python REPL tool, capability gate | Extensions yes; API no | P2.x |
| `profiles` | Loading/merging/validating model profiles, agent profiles, project overrides; catalog; bundles | Content yes; loader no | P1.8 |
| `sandbox` | Inner bwrap policy derivation from capability declarations; backends; launchers | Policy yes; enforcement no | P1.7 |
| `orchestrator` | Placement, wakers, fleet, agent-to-agent messaging, trust tiers | No | P1.9 (seed), P3.3–P3.7 |
| `provenance` | Event-log projector → relational PROV schema; artifact store | Schema no | P2.5, P3.1–P3.2 |
| `evolve` | Outer loop: proposer harness, eval runner, promotion gates | Yes (it's a profile too) | P4 |

The `ui` client (Tauri app / web build) is not a workspace crate; it lands in P1.9
under its own directory once ADR-0004 picks the wire format.

## Content directories

`profiles/` at the repository root holds the mutable profile content (`catalog.toml`,
`bundles.toml`, `models/`, `agents/`) that `crates/profiles` loads; `crates/sandbox/src/tools/`
holds the six base tools (P1.7) until the `ext` API lands in P2.1.

## Spikes

`spikes/` holds throwaway Phase 0 code. It is excluded from the workspace
(`Cargo.toml` → `[workspace] exclude`) and is never built by CI's workspace jobs.
