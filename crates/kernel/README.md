# `kernel`

**Responsibility:** Agent loop, typed State, Tool trait, Middleware trait, event log, checkpoints, suspend/resume.

**Mutable by the evolve loop?** **No** (frozen; soft freeze at P1 exit, hard freeze at P2 exit, D12). Depends on no other in-repo crate; a test enforces this.

See the crate table in `docs/agent-harness-dev-plan.md` §3.1 and the dependency
DAG in `crates/README.md`. This crate is a stub until the milestone that fills it
in (see `docs/IMPLEMENTATION_PLAN.md`).
