# Contributing

Three rules govern this repository. They are short because they are absolute.

## 1. The kernel boundary

The single most important line in the system is *frozen kernel / mutable
everything else* (dev plan §3.1, §14; decision D12).

- `crates/kernel` defines the loop, `State`, the `Tool` / `Middleware` /
  `Provider` / `Host` / `ArtifactStore` / `Memory` / `SandboxBackend` traits, the
  event log, checkpoints, and suspend/resume. Nothing else.
- `kernel` depends on no other in-repo crate. A test enforces this
  (`crates/kernel/tests/no_in_repo_deps.rs`). See `crates/README.md` for the DAG.
- **After Phase 1 exit, any change to `crates/kernel` requires an ADR** linked from
  the PR. After Phase 2 exit, kernel changes are exceptional and the ADR must say
  why the change cannot live in `ext`, a profile, or a policy file.
- If a feature *could* be middleware, a tool, a profile field, or a sandbox
  policy, it is not a kernel feature.

## 2. `docs/design-decisions.md` is binding

Decisions D1–D20 were made before Phase 0 and implementing agents (human or not)
honor them as written. A task tagged `(D7)` in the plan must satisfy D7.

If you find a decision unworkable: **stop, open an ADR** under `docs/adr/`
proposing the change, and wait for it to be accepted. Do not improvise around it.
The ADR template is `docs/adr/0000-template.md`; add a row to `docs/adr/README.md`
and to the decision log in `docs/IMPLEMENTATION_PLAN.md`.

## 3. Progress is tracked in one place, ticked on merge

`docs/IMPLEMENTATION_PLAN.md` is the single source of truth for progress.

- Update it **in the same PR** as the work it tracks: tick the task boxes the PR
  completes, set the milestone status, and fill in the progress summary table.
- A box means *merged to `main` and verified*. A PR ticks the boxes for the work
  it carries; the tick becomes true when the PR merges. Never tick work that is
  merely started or that lives on another branch.
- A milestone that claims a behavioral property is done only when a test asserts
  that property. Nothing is promoted on faith.
- Items marked 🧑 need a human (access, review, infrastructure). Agents leave
  them unticked and say so in the PR.

## Toolchain and checks

- Toolchain is pinned in `rust-toolchain.toml`; MSRV is `workspace.package.rust-version`
  in `Cargo.toml`. Bump both together and record it in the plan's change log.
- Every PR must pass `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`, and `cargo test --workspace --all-features`.
- Spike code goes under `spikes/`, is excluded from the workspace, and is never
  imported by a crate.

## Conventions

- Reference milestone IDs (`P1.3`) in branch names, PR titles, and ADRs.
- `docs/specs/` and the code that implements a spec change land in the same PR.
- Each crate has a `README.md` stating its responsibility and whether the evolve
  loop may mutate it.
- Commit messages: imperative subject line, milestone ID where applicable.
