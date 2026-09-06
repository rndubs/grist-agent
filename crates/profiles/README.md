# `profiles`

**Responsibility:** Loading, merging, and validating model profiles, agent profiles, and
project overrides; the catalog; the capability bundles file.

**Mutable by the evolve loop?** Content (`profiles/**`) yes; the loader (this crate) no.

Contract: `docs/specs/profile-schema.md` (D6, D7, D16). Kernel types it produces:
`docs/specs/kernel-interface.md` §3.2–3.4, §3.7, §3.8, §3.12, §9.

## Shape

| Module | What it owns |
|---|---|
| `diagnostic` | `Diagnostic { code, file, toml_path, message }`; `Display` is `<code> at <file-basename>:<toml_path>: <message>` (§7.3) |
| `atom` | TOML string form of capability atoms (§11 grammar), placeholders `${workdir}` / `${home}` / `${install:<tool>}` (§7.4), expansion + normalization into `kernel::Capability` (§11.3), `symbolize` for messages |
| `schema` | Per-file validation on the parsed `toml::Table` (§7.2 step 2): a static schema per file kind (model, agent, project, bundles, catalog) walked for unknown / kernel-only keys and types with exact TOML paths, then the semantic rules (ranges, grammars, text sources, middleware slots, layer-3 forbidden keys, `env_allow` secret patterns) |
| `merge` | Layer merge with provenance (§6): scalars override, tables deep-merge, lists replace, type mismatch → later wins, `[[middleware]]` keyed by name (priority + config replaced, no deletion); `origin_of(path)` names the file that set a value |
| `bundles` | `Bundles::load`, `expand` / `expand_indexed` (§5) |
| `catalog` | `Catalog::load` (§10.1 rules), `entries`, `get`; `discover_profiles_dir` |
| `resolved` | `ResolvedProfile` and its sections (§7.6): symbolic paths, text sources resolved to text, `fine_tune` excluded |
| `resolve` | `resolve(&ResolveInputs) -> Result<Resolved, Vec<Diagnostic>>`, `Registry`, `ToolDecl`, `KernelInputs` |
| `prompt` | System prompt blocks (§8): order, headers, trimming, per-block hashes |
| `lib` | `kernel_defaults()` — layer 0, exactly §1.4 as TOML |

## Resolution (§7.2), as implemented

`resolve` runs the steps in order and stops at the first step that produces an error,
reporting every error of that step:

1. **Load** `catalog.toml`, `bundles.toml`, the entry's model and agent files,
   `<workdir>/.grist/agent.toml` if present; hash the raw bytes (`Hash::of_bytes`).
2. **Validate each file** against its own schema (`schema::validate_file`); text sources are
   read here (relative to the containing file) and relative paths are made absolute.
3. **Merge** layers 0 → 1 → 2 → 3 → 4 (`merge::Layered`); layer 4 is the §4.4 whitelist; then
   the required keys (`model.id`, `model.endpoint`, `model.context_length`, `agent.name`) and
   the cross-key rules (compaction, thinking budget).
4. **Expand bundles** in grants, MCP capabilities, sub-agent ceilings; add derived `tool:` /
   `spawn:` atoms; check sub-agent catalog references.
5. **Parser slot** synthesis (`tool_call_parser@100` for `parsed:<syntax>`), registry checks
   (parser, middleware names, memory module, sandbox backend incl. `E_SANDBOX_NONE_FORBIDDEN`,
   tools, `tool_descriptions` keys), `recorder@990` appended, stable sort.
6. **`resolved_profile_hash`** = `Hash::of_canonical_json(&ResolvedProfile)` — before
   expansion, so the hash is portable across machines and workdirs.
7. **Expand placeholders** and normalize (`E_UNKNOWN_PLACEHOLDER`, `E_PATH_NOT_ABSOLUTE`,
   `E_PATH_DOTDOT`); parse atoms into `kernel::Capability`; `W_NET_MASKED`.
8. **Narrowing** with `Capability::narrower_than`: tool declarations, MCP servers
   (+ `E_MCP_URL_HOST`), sub-agent ceilings and the child's layers-0–2 grants (memoized on
   catalog name, recursion terminates), `notebook.path` as `fs.rw`,
   `E_HIDDEN_EVAL_REACHABLE`, then the layer-3 rules by resolving layers 0–2 again and
   comparing (`E_OVERRIDE_WIDENS_GRANTS`, `E_OVERRIDE_WIDENS_SANDBOX`).
9. **Envelope**: `derive_policy_with(grants − secret atoms, grants, limits)` and its hash.
10. **System prompt**: model, role, `AGENTS.md` (+ notebook on resume) blocks.
11. **`profile_loads`**: bundles, model, agent, project (if present), each `rejected: false`,
    `turn: 0`; `drift` when `[fine_tune]` disagrees with the resolved hash.
12. **`active_profiles`** for `State.profiles`; `kernel_inputs` for `KernelConfig`.

## What the launcher consumes

`Resolved { resolved_profile, resolved_profile_hash, active_profiles, catalog_hash,
profile_loads, warnings, drift, kernel_inputs }`. `warnings` are `W_*` diagnostics
(`warning{class: "profile_warning", detail: {code, toml_path}}`); `drift` maps to
`warning{class: "profile_drift", detail: {expected, actual}}`. `KernelInputs` carries
`model_id`, `endpoint`, `tool_format`, `context_length`, `quirks`, `model_params`,
`system_prompt`, `middleware` (name/priority/source/config/config_hash — the launcher
instantiates by name), `grants`, `tools`, `tool_descriptions`, `sandbox_backend`,
`sandbox_limits`, `spill`, `context_budget_tokens`, `compaction`, `notebook_path`, `notebook`,
`envelope_policy`, `sandbox_policy_hash`, `agents_md_path`, `skills_paths`, `mcp_servers`,
`subagents`, `memory`, `eval_set`, `catalog_entry`.

## Adding a profile

1. Model: `profiles/models/<endpoint>-<model>.toml` with `[model]` (`id`, `endpoint`,
   `context_length` required), optional `[prompt]`, `[tool_descriptions]`, `[compaction]`,
   `[[middleware]]` at 100–199, `[fine_tune]` for fine-tunes.
2. Agent: `profiles/agents/<name>.toml` with `[agent].name = "<name>"`, `[capabilities].grants`
   (atoms and bundle names; never `tool:` / `spawn:`), `[tools].allow`, and the optional tables
   of §3; middleware at 200–899.
3. Register the pair in `profiles/catalog.toml` under `[[agents]]` with `name = "<name>"`.
4. `cargo test -p profiles` — the shipped tree is resolved end to end by `tests/shipped.rs`;
   validator behaviour is pinned one-for-one to §9 by `tests/validator.rs`.

Project overrides live at `<workdir>/.grist/agent.toml` and may only tune and narrow (§4.2).
