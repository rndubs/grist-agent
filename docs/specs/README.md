# Interface specs (P1.0, D20)

Agents implement Phase 1 against these signatures, not against prose. A spec change and the
code that needs it land in the same PR (`CONTRIBUTING.md`, "Conventions"). Each spec is
reviewed by a human before P1.1 starts (`docs/IMPLEMENTATION_PLAN.md`, P1.0). All three were approved at v0.1 on 2026-09-06; later edits are spec changes that land with the code that needs them.

| Spec | Covers | Decisions | Version | Review status |
|---|---|---|---|---|
| [`kernel-interface.md`](./kernel-interface.md) | Rust signatures for every `kernel` type and trait; task state machine; session state machine; the loop; cancellation, retry, crash-recovery, spill, secrets, middleware-chain, and tool-registry semantics | D1, D2, D4, D5, D6, D10, D12, D14, D15, D17 | 0.1 (2026-09-06) | approved (2026-09-06) |
| [`event-schema.md`](./event-schema.md) | Event envelope and JSONL framing; every event kind with payload fields and examples; hash definitions and the volatile-field lists; redaction; record/replay mapping; checkpoint/restore; provenance mapping hint | D3, D10, D13, D15, D16 | 0.1 (2026-09-06) | approved (2026-09-06) |
| [`profile-schema.md`](./profile-schema.md) | TOML schema for model profiles, agent profiles, project overrides, and the bundles file; merge rules; middleware priority slots; system prompt block order; validator rejection cases; capability atoms and `narrower_than` | D6, D7, D16 | 0.1 (2026-09-06) | approved (2026-09-06) |
| [`protocol.md`](./protocol.md) | ACP v1 as served by `grist-kernel`/`grist-daemon`: methods and their log effects, the `_grist/*` extension namespace (`subscribe`, `event`, `cancel`, `status`), the pure `Event` → `session/update` projection, socket auth, processes and state directory | ADR-0004, D4, D11, D15, D17 | 0.1 (2026-09-06) | normative for P1.9 (2026-09-06) |

Shared conventions (hash strings, capability atom strings, task and session state names, the
event envelope, content block shapes) are stated identically in all three documents; if two
disagree, that is a bug to fix in the same PR, not a precedence question.

Each spec ends with a numbered list of open questions for the reviewer: decisions the spec
makes that D1–D20 do not literally settle.
