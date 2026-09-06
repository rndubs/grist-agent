# Architecture Decision Records

Any change to the `kernel` crate after Phase 1 requires an ADR. Everything else belongs in `ext`, a profile, or a policy file (see the kernel-boundary rule in `docs/IMPLEMENTATION_PLAN.md`).

Copy `0000-template.md`, number sequentially, and add a row here.

| ADR | Title | Status |
|---|---|---|
| 0000 | Template | n/a |
| 0001 | Out-of-process tool mechanism (process JSON-RPC over WASM), recording the D8 tier split | accepted (2026-09-06) |
| 0003 | Provider client shape: one OpenAI-compatible client with quirk flags | accepted (2026-09-06) |
| 0004 | Wire protocol for the daemon and the first client: adopt ACP v2 with a `_grist/*` extension namespace | accepted (2026-09-06) |
