# grist-agent

A minimal, embeddable, self-improvable agent kernel for simulation and engineering workflows, written in Rust.

## Documents

- [`docs/agent-harness-dev-plan.md`](docs/agent-harness-dev-plan.md) — the design: goals, architecture, and rationale.
- [`docs/IMPLEMENTATION_PLAN.md`](docs/IMPLEMENTATION_PLAN.md) — phased milestones with checkboxes and exit criteria. This is where progress is tracked.
- [`docs/design-decisions.md`](docs/design-decisions.md) — twenty binding decisions (D1–D20) made before Phase 0; the plan references them by number.
- [`docs/adr/`](docs/adr/) — architecture decision records.

## Status

Phase 1 (kernel + local daemon) is nearly complete: the kernel (`crates/kernel`: types, loop, event log, record/replay), `providers`, `host`, `sandbox` with the six base tools, `profiles` with the in-repo default agent under `profiles/`, and the P1.9 protocol server in `orchestrator` (`grist-kernel` over stdio, `grist-daemon` over a unix socket, `grist-connect` for editors; ACP per ADR-0004) have landed with their tests, and four of the five Phase 1 exit criteria are met by end-to-end sessions in `crates/orchestrator/tests/`. What remains in Phase 1 is a human at an editor: the Zed session described in `docs/clients/zed.md`, then the kernel soft-freeze. Phase 0 items that need the HPC login node (P0.1, ADR-0002) or real model endpoints (P0.2) remain with their human owners. See the progress summary at the top of the implementation plan.

The specs under `docs/specs/` are the normative surface: `kernel-interface.md` (types, traits, loop semantics), `event-schema.md` (envelope, event kinds, hashes, redaction, replay), `profile-schema.md` (TOML profiles, bundles, validator), `protocol.md` (ACP methods, the `_grist/*` namespace, the event projection).

## Building

```
cargo build --workspace
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

`--all-features` enables the `None` sandbox backend (`dev-sandbox-none`, development builds only, D14) that the integration tests use where `bwrap` is unavailable, and the opt-in provider integration tests (`standin-integration`, which read the `STANDIN_*` variables of `docs/standin.md`).

The toolchain is pinned in `rust-toolchain.toml`. See `CONTRIBUTING.md` for the rules and `crates/README.md` for the crate layout.
