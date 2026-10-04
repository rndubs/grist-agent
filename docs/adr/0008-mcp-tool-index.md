# ADR-0008: MCP lazy exposure keeps a tool index in the prompt

- **Status:** proposed
- **Date:** 2026-10-04
- **Milestone:** P2.2 (see `docs/IMPLEMENTATION_PLAN.md`)

## Context

The dev plan (§7 item 2) and `profile-schema.md` §3.4 as approved at v0.1 kept MCP tool schemas out of the prompt (`lazy = true`) and gave the model a `find_tools(query)` tool that exposed matches "for the next turn only". The P2.2 test required prompt size to be independent of the number of registered servers.

That leaves the model with nothing to search for. With no MCP server named anywhere in its context, it has no reason to call `find_tools` and no vocabulary for the query. Lazy loading in the systems we compared against (Claude Code's deferred tools, the Anthropic API's tool search) keeps a short **index** of names in the prompt and loads full schemas on request. That is the part that makes it work.

This has to be settled before P2.2 starts, because it changes a profile key, a binding decision (D7's system prompt block order), and the kernel's `PromptBlockKind` enum. The enum change is cheapest before the Phase 1 soft freeze.

## Options considered

1. **Search only (as approved).** No per-server text in the prompt. Cheapest, but the model searches blind and in practice won't use tools it hasn't heard of.
2. **Index of tool names, schemas on demand.** One line per server plus the names of its tools. Costs a few tokens per tool, independent of schema size.
3. **Index of servers only, names and schemas on demand.** One line per server. Cheapest that still tells the model what exists; needs a good server description.
4. **Everything in the prompt.** Every schema sent every turn. The context-flooding failure the plan is designed to avoid.

## Decision

Make the choice per server with a new `index` key in `[[mcp_servers]]`, replacing `lazy`:

- `"names"` (default): option 2.
- `"server"`: option 3, for servers with many tools.
- `"full"`: option 4, kept for small servers that are used every turn (what `lazy = false` meant).

A new `description` key supplies the server's index line. The index is a new system prompt block, `tool_index`, fixed at position 4 in the D7 order: after `AGENTS.md`, before active skills. It is per session, so it sits in the stable prefix ahead of the per-turn blocks. A tool returned by `find_tools` or named by an active skill stays exposed until the next compaction rather than for one turn only. Exposure is recomputed from `State.messages`, so it needs no new state and replays exactly. The full rules are `profile-schema.md` §3.4.1.

## Consequences

- **D7 is amended:** the block order becomes model prompt variant, agent role prompt, `AGENTS.md`, tool index, active skills, notebook.
- **Kernel:** `PromptBlockKind` gains `ToolIndex` (serialized `tool_index`). This is an additive enum change, made now so the soft freeze doesn't need another ADR for it. `kernel-interface.md` and `event-schema.md` list it.
- **Profiles:** `lazy` becomes an unknown key, and `index` and `description` are added. Under `profile-schema.md` §12 renaming a key bumps `schema_version`. It stays at `1` because no released profile can use `[[mcp_servers]]` yet: P2.2, the only consumer, hasn't started, and nothing shipped sets the key. This is recorded as a pre-implementation amendment, not a precedent.
- **P2.2's budget test changes** from "independent of N servers" to "grows by at most the index lines per added server, and not at all with the tool count of a `"server"` server".
- Server descriptions are now part of the prompt, so their wording matters. `W_MCP_NO_DESCRIPTION` warns when a `"server"` entry has none.
- Ranking quality for `find_tools` stays an implementation detail behind an interface. Keyword match ships in P2.2.
