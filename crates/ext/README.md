# `ext`

**Responsibility:** Extension API and first-party extensions: MCP client, sub-agent spawn, skills loader, memory modules, workflow runner, Python REPL tool, capability gate.

**Mutable by the evolve loop?** Extensions yes; the API no.

## P2.1: the extension API

Spec: `docs/specs/extension-manifest.md`. Two tiers (D8, ADR-0001):

| Tier | What | Module | Authored by the agent? |
|---|---|---|---|
| Compiled | Rust middleware and first-party tools: `Extension` → `ExtensionSet` (tools, validator declarations, middleware by name). The six base tools are registered as `BaseTools`. | `compiled` | never |
| Out-of-process | A directory with `extension.toml` (name, version, kind, command, required capabilities, tools). `load_all` admits it only if the profile's grants cover every capability; its tools run as `ext.<name>.<tool>` under the Stateless or Session sandbox launcher, speaking the stdio subset of MCP (`tools/call`). | `manifest`, `loader`, `process` | yes |

A profile loads out-of-process extensions with `[extensions].paths` (`profile-schema.md`
§3.12); the launcher (`crates/orchestrator/src/launcher.rs`) does the loading. The example is
`examples/text-stats/`.

Depends on `kernel` and `sandbox` (the base tools; `crates/README.md`).
