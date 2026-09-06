# grist-agent

A minimal, embeddable, self-improvable agent kernel for simulation and engineering workflows, written in Rust.

## Documents

- [`docs/agent-harness-dev-plan.md`](docs/agent-harness-dev-plan.md) — the design: goals, architecture, and rationale.
- [`docs/IMPLEMENTATION_PLAN.md`](docs/IMPLEMENTATION_PLAN.md) — phased milestones with checkboxes and exit criteria. This is where progress is tracked.
- [`docs/design-decisions.md`](docs/design-decisions.md) — twenty binding decisions (D1–D20) made before Phase 0; the plan references them by number.
- [`docs/adr/`](docs/adr/) — architecture decision records.

## Status

Phase 0 (spikes) is in progress: P0.0 repository foundations are done. See the progress summary at the top of the implementation plan.

## Building

```
cargo build --workspace
cargo test --workspace
```

The toolchain is pinned in `rust-toolchain.toml`. See `CONTRIBUTING.md` for the rules and `crates/README.md` for the crate layout.
