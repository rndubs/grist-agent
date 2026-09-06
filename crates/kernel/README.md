# `kernel`

**Responsibility:** Agent loop, typed State, Tool trait, Middleware trait, event log, checkpoints, suspend/resume.

**Mutable by the evolve loop?** **No** (frozen; soft freeze at P1 exit, hard freeze at P2 exit, D12). Depends on no other in-repo crate; a test enforces this.

See the crate table in `docs/agent-harness-dev-plan.md` §3.1 and the dependency
DAG in `crates/README.md`. The public surface is `docs/specs/kernel-interface.md`;
events and hashes are `docs/specs/event-schema.md`.

## Layout

| Module | Spec | Milestone |
|---|---|---|
| `hash` (+ `hash::strict`) | §3.1, `event-schema.md` §3.8: `Hash`, RFC 8785 canonicalizer | P1.1 |
| `capability` | §3.2: atoms, `narrower_than`, property tests | P1.1 |
| `content`, `task`, `state` | §3.3–§3.5: blocks, messages, `State`, migrations | P1.1 |
| `tool`, `middleware`, `provider` | §3.6–§3.8: traits and contexts | P1.1 |
| `host`, `artifact`, `memory`, `sandbox` | §3.9–§3.12: traits, no-op impls, `derive_policy` | P1.1 |
| `cancel`, `event`, `redact`, `config` | §3.13–§3.15: tokens, the 28 event kinds, redactor, plain config types | P1.1 |
| `log` (`MemoryEventLog`, reader helpers) | §3.14, `event-schema.md` §6 | P1.1 |
| `log::file` (`FileEventLog`, JSONL, fsync) | §3.14, `event-schema.md` §1 | P1.3 |
| `loop_` | §3.15, §4–§7: `Kernel`, `KernelHandle`, the loop | P1.2 |
| `replay` (+ `bin/diff-logs`) | §3.16, `event-schema.md` §5 | P1.4 |
