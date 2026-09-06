# `providers`

**Responsibility:** Model clients. v0: one OpenAI-compatible client with per-endpoint quirk flags (covers vLLM, LiteLLM).

**Mutable by the evolve loop?** No.

See the crate table in `docs/agent-harness-dev-plan.md` §3.1 and the dependency
DAG in `crates/README.md`. This crate is a stub until the milestone that fills it
in (see `docs/IMPLEMENTATION_PLAN.md`).
