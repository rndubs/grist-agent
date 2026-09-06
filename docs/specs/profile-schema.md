# Profile and capability schema

| | |
|---|---|
| **Status** | draft, awaiting human review (P1.0 🧑) |
| **Version** | 0.1 |
| **Date** | 2026-09-06 |
| **Decides for** | P1.8 (`profiles` crate, catalog, validator), and the profile-facing halves of P1.7, P2.1–P2.4, P2.6–P2.9, P4.4, P5.1 |
| **Binding decisions** | D6, D7 above all; also D1, D5, D8, D9, D10, D12, D13, D14, D16, D18, D19 |
| **Sibling specs** | `kernel-interface.md` (owns `kernel::Capability`, `State`, `SandboxPolicy`, `derive_policy`, `Tool`, `Middleware`), `event-schema.md` (owns the envelope, event kinds, `request_hash`, `state_hash`) |

This document is the contract for everything that lives in `profiles/` and `.grist/`. P1.8 implements it; the validator tests in §9 are derived from it one-for-one. Where this document names a kernel type it uses the name from `kernel-interface.md` and does not redefine it. Where it says **MUST** / **MUST NOT** the validator rejects; where it says **SHOULD** the validator warns.

Conventions shared with the sibling specs:

- Hash strings are `b3:<64 lowercase hex>` — BLAKE3 over RFC 8785 canonical JSON (D3).
- Capability atoms are the Rust enum `kernel::Capability` (D6). This spec owns their **TOML string form** (§11), the bundle expansion (§5), and the placeholder expansion (§7.4). The kernel and sandbox see only atoms.
- `profile_load` event payload is `{ kind: model|agent|project|bundles|catalog|skill|subagent, path, hash, rejected }` (event-schema.md §2.3; `catalog` and `subagent` are emitted by P1.8 and P2.4 respectively). This document writes it as `ProfileLoad{kind:"…"}` for brevity.

---

## 1. Purpose, files, and precedence

### 1.1 What profiles are

Two orthogonal axes over the frozen kernel, both TOML, both versioned, both content-hashed (dev plan §6):

- a **model profile** packages everything specific to one served model: prompt variant, tool-description phrasing, tool-call format, sampling defaults, context length, compaction thresholds, provider quirks, and (for fine-tunes) the profile hash the weights were trained against;
- an **agent profile** packages everything specific to one role: capabilities, tool allowlist, MCP servers, skills, sub-agents, middleware chain, memory module, sandbox limits, notebook, eval-set pointer.

A **task agent** is a `(model profile, agent profile)` pair named in the **catalog**. A **project override** is a small file inside the working tree that narrows and tunes an agent for one repository. **Profiles override values, never structure** (dev plan §6): no profile can replace the loop, remove a kernel behaviour, or widen the sandbox beyond what the layer below it granted.

### 1.2 Layers and precedence

Resolution applies five layers in this order; later layers win under the merge rules of §6:

| # | Layer | Where it lives | Who may write it | Trust |
|---|---|---|---|---|
| 0 | **Kernel defaults** | Rust code in `profiles` (`profiles::kernel_defaults()`), shown as TOML in §1.4 | kernel developers, via ADR after P1 exit | frozen |
| 1 | **Model profile** | `profiles/models/<name>.toml` (in-repo), or any path the catalog names | harness developers; the evolve loop (P4) | mutable layer |
| 2 | **Agent profile** | `profiles/agents/<name>.toml` (in-repo), or any path the catalog names | harness developers; the evolve loop (P4) | mutable layer |
| 3 | **Project overrides** | `<workdir>/.grist/agent.toml` | the repository's owners — and therefore **the agent itself**, since the workdir is `fs.rw` | narrowing-only (§4) |
| 4 | **Runtime overrides** | the protocol's start-session message (P1.9) | the client | whitelist of six keys (§4.4) |

The trust column is the reason §4 exists: layer 3 sits inside the sandbox's writable area, so anything security- or eval-relevant is not overridable there, and layer 4 comes from an arbitrary client, so it is a fixed whitelist.

Supporting files, loaded alongside the layers:

| File | Location | Purpose |
|---|---|---|
| **Bundles** | `profiles/bundles.toml` (exactly one, in-repo) | named atom sets (`meshing`, `solver`, `post`) and the in-house install paths (§5) |
| **Catalog** | `profiles/catalog.toml` (exactly one, in-repo) | task-agent registry: name → (model profile path, agent profile path) (§10) |
| **Prompt and role files** | relative to the profile file that names them | text sources (§2.3) |
| **`AGENTS.md`** | `${workdir}/AGENTS.md` by default | project instructions block (§8) |
| **Skills** | directories named by `[skills].paths` | markdown-with-frontmatter, loaded per turn (P2.3) |

`profiles/` is a top-level repository directory, sibling to `crates/`. Everything under it is "content" the evolve loop may mutate (crates/README.md); the loader in `crates/profiles` is not.

### 1.3 How the catalog finds files

1. The kernel process is started with a `--profiles-dir <path>` (default: the `profiles/` directory adjacent to the binary's repository root, discovered by walking up from the executable until a `profiles/catalog.toml` is found) and a `--workdir <abs-path>` (from the protocol start-session message).
2. `catalog.toml` and `bundles.toml` are read from `--profiles-dir`. Both are required; a missing file is a fatal load error, not a validator error.
3. The start-session message names a catalog entry; absent, the entry named `default` is used and MUST exist.
4. The entry's `model_profile` and `agent_profile` paths are resolved relative to `catalog.toml`'s directory. Absolute paths are permitted (the eval runner uses them for candidate profiles, P4.2).
5. `<workdir>/.grist/agent.toml` is loaded if present. Absence is normal and is not logged as an error.
6. Paths inside a profile (`prompt.file`, `agent.role_prompt.file`, `skills.paths`, `subagents[].definition`) are resolved relative to the directory of the file that contains them, then made absolute. Placeholder-bearing paths (§7.4) are expanded first.

### 1.4 Kernel defaults (layer 0), as TOML

This is the exact content `profiles::kernel_defaults()` produces, and it is what a model or agent profile inherits when it omits a key. Keys marked *required* have no default and MUST be present after merging layers 1 and 2.

```toml
schema_version = 1

[model]
# id              (required, string)   full endpoint model string, opaque to the kernel
# endpoint        (required, string)   endpoint name, resolved by the launcher (§2.1)
# context_length  (required, integer)  tokens
tool_format = "native"
max_output_tokens = 4096
# temperature: absent means "do not send the parameter; the server decides"

[model.thinking]
enabled = false
budget_tokens = 0

[model.quirks]
reasoning_field = "none"
supports_structured_output = false
supports_stream_usage = false
strict_tool_schema = false
auth = "none"

[compaction]
trigger_at = 0.85
target = 0.50

[agent]
# name        (required, string)
context_budget_tokens = 40000       # D16 placeholder, to be tuned
spill_cap_bytes = 16384             # D12: spill is a kernel behaviour; only the cap is a value
agents_md = "${workdir}/AGENTS.md"

[capabilities]
grants = []

[tools]
allow = []

[skills]
paths = []

[memory]
module = "none"

[sandbox]
backend = "bwrap"
timeout_s = 600
scratch_tmpfs_mb = 256
env_allow = ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"]
network = false

[notebook]
path = "${workdir}/.grist/notebook.md"
inject_on_resume = true
max_tokens = 4000
```

The kernel also contributes middleware entries that never appear in any file (§2.7): they carry `source = "kernel"` in the resolved chain.

---

## 2. Model profile schema

One file per served model. File name convention: `profiles/models/<endpoint>-<short-model>.toml`; the name is informational, the catalog path is authoritative.

### 2.1 `[model]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | The full model string sent to the endpoint: bare served name for vLLM (`qwen3-32b-ft-2026-08`), `provider/model` for LiteLLM (`anthropic/claude-sonnet-4-5`, `stand-in/default`). Opaque to the kernel; recorded in every model-call event (D13). |
| `endpoint` | string, `[a-z][a-z0-9-]*` | yes | Name of the endpoint. **The base URL is deliberately not in the profile** so that moving an endpoint does not change any profile hash (and therefore cannot trip the P5.1 drift warning). In P1 the launcher resolves `endpoint` to a base URL from the environment variable `GRIST_ENDPOINT_<NAME>_URL` with `<NAME>` upper-cased and `-` → `_` (e.g. `litellm-ci` → `GRIST_ENDPOINT_LITELLM_CI_URL`). A file-based endpoint registry is a `host` / P1.9 concern and out of scope here. A missing variable is a fatal start error, not a validator error. |
| `context_length` | integer > 0 | yes | Context window in tokens. Read by compaction (P2.6) and budget instrumentation (P2.9). |
| `tool_format` | `"native"` or `"parsed:<syntax>"` | no (default `"native"`) | `native`: the endpoint returns structured tool calls. `parsed:<syntax>`: tool calls arrive as text and the parser middleware (§2.7) in the fixed slot normalizes them. `<syntax>` is `[a-z][a-z0-9_-]*` and MUST be in `Registry.parsers` (§7.1). |
| `temperature` | float in [0, 2] | no | Absent means the parameter is not sent. |
| `max_output_tokens` | integer > 0 | no (default 4096) | Sent as `max_tokens` / `max_completion_tokens` per the provider client's mapping. |

#### `[model.thinking]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `enabled` | bool | `false` | Request extended reasoning where the endpoint supports it. |
| `budget_tokens` | integer ≥ 0 | `0` | Reasoning budget; `0` means "provider default". MUST be `0` when `enabled = false`. |

#### `[model.quirks]`

A closed table mirroring the per-endpoint quirk flags of the `providers` crate (dev plan §5, P0.2, P1.5). Unknown keys are rejected like anywhere else; when P0.2 discovers a new quirk it is added here **and** in `providers` in the same PR (CONTRIBUTING.md, "spec and code land together").

| Key | Type | Default | Meaning |
|---|---|---|---|
| `reasoning_field` | string | `"none"` | Name of the response field carrying reasoning text (`"reasoning_content"` for vLLM, `"reasoning"` for some LiteLLM upstreams). The provider maps it into a `Thinking` content block. `"none"` means the endpoint emits no such field. Any other value MUST match `[a-z][a-z0-9_]*`. |
| `supports_structured_output` | bool | `false` | JSON-schema / guided decoding is available. When `false`, structured-output middleware falls back to prompt-and-parse. |
| `supports_stream_usage` | bool | `false` | The endpoint emits a usage object in the final SSE chunk (`stream_options.include_usage`). When `false` the provider issues no follow-up and records usage as `null`; the D13 usage fields are then absent, not zero. |
| `strict_tool_schema` | bool | `false` | The endpoint validates tool-call arguments against the schema (`strict: true`). When `true` the provider sends `additionalProperties: false` schemas. |
| `auth` | `"none"` or `"bearer:<SECRET_NAME>"` | `"none"` | How to authenticate. `<SECRET_NAME>` is `[A-Z][A-Z0-9_]*` and is resolved through `Host` secret handles at request time, inside the kernel process, by the provider client only (D10). **It is not a capability atom**: `secret:` atoms (§11) govern what tools and MCP servers may see; the provider's own credential never enters a sandbox. |

### 2.2 `[prompt]`

The model-specific system prompt variant — block 1 of §8. It is a **text source** (§2.3). Omitting the table omits block 1.

```toml
[prompt]
text = """You are a careful engineering assistant. Prefer small, verifiable steps.
When a tool exists for an action, call it instead of describing it."""
```

or

```toml
[prompt]
file = "prompts/qwen3-default.md"
```

### 2.3 Text sources

Every prose field (`prompt`, `agent.role_prompt`, each `tool_descriptions.<tool>`) is a *text source*: either a TOML string (inline) or a table with exactly one key `file` (a path, relative to the containing profile file). Both forms MUST NOT be combined; a table with any key other than `file`, or a `file` that does not exist at load time, is rejected.

Merge behaviour (§6.1): when two layers provide the same text source in different shapes (one inline, one `{ file = … }`) the later layer replaces the earlier one entirely — this is the type-mismatch rule and is why the file form is a single-key table and not a sibling key.

Text is loaded verbatim, UTF-8, without trailing-whitespace normalization; placeholders are **not** expanded inside prose (§7.4), so a prompt may say `${workdir}` literally.

### 2.4 `[tool_descriptions]`

Per-tool phrasing overrides, keyed by tool name (§11.5 convention). Each value is a text source. A key that names a tool absent from `Registry.tools` is rejected (`E_TOOL_UNKNOWN`) — a model profile cannot describe a tool that does not exist in this build. Descriptions here replace the tool's compiled default description in the request; the tool's schema is never changed by a profile.

```toml
[tool_descriptions]
bash = "Run a shell command in the sandbox. State does not persist between calls."
python = { file = "descriptions/python-terse.md" }
```

### 2.5 `[compaction]`

Thresholds as fractions of `model.context_length` (P2.6 reads them; P4.6 evolves them).

| Key | Type | Default | Constraint |
|---|---|---|---|
| `trigger_at` | float | `0.85` | `0 < target < trigger_at ≤ 1.0` |
| `target` | float | `0.50` | fraction to compact *down to* |

### 2.6 `[fine_tune]`

Present only for fine-tuned models (P5.1).

| Key | Type | Required | Meaning |
|---|---|---|---|
| `trained_against_profile_hash` | hash string | yes | The `resolved_profile_hash` (§7.6) of the task agent whose traces trained these weights. |
| `warn_on_drift` | bool | no (default `true`) | Emit the drift warning (below) when the hash does not match. `false` silences it for a deliberately re-targeted fine-tune; the mismatch is still recorded. |

Drift rule: after resolution, if `fine_tune.trained_against_profile_hash != resolved_profile_hash`, the loader emits a `warning` event with `class = "profile_drift"` and `detail = { expected, actual }` (event-schema.md §2.28) and the protocol surfaces it to the UI (P5.1). To keep the comparison well-founded, **the `fine_tune` table is excluded from the struct that `resolved_profile_hash` covers** (§7.6); otherwise pinning the hash would change the hash.

### 2.7 `[[middleware]]` and priority slots

Middleware are compiled Rust (D8) selected and configured by name. Each entry:

| Key | Type | Required | Meaning |
|---|---|---|---|
| `name` | string, `[a-z][a-z0-9_]*` | yes | MUST be in `Registry.middleware`, MUST NOT be a kernel-reserved name. |
| `priority` | integer | yes | Sort key; MUST lie in the range allowed for the file kind (table below). |
| `config` | table | no (default `{}`) | Opaque to the profile validator; the middleware validates it at construction. This is the **one place unknown keys are not rejected by the profile validator**. |

**Priority ranges** (D7: "the model profile's tool-call parser has a fixed early slot"):

| Range | Owner | Contents |
|---|---|---|
| 0–99 | kernel, reserved head | inserted from code, never from a file (`replay` at 0 when replaying, P1.4) |
| 100–199 | model profile | `tool_call_parser` at **exactly 100** when `tool_format = "parsed:<syntax>"`; other model-side normalizers at 101–199 |
| 200–899 | agent profile (and project overrides of agent-profile entries) | everything else: compaction (P2.6, suggested 300), memory (P2.7, suggested 400), find-tools exposure (P2.2, suggested 500), budget instrumentation (P2.9, suggested 800) |
| 900–999 | kernel, reserved tail | `recorder` at 990 (P1.4): it must see the request exactly as sent |

Kernel-reserved names: `recorder`, `replay`. A file using them is rejected (`E_RESERVED_MIDDLEWARE_NAME`).

**The parser slot.** When `model.tool_format = "parsed:<syntax>"`, the resolver synthesizes the entry `{ name = "tool_call_parser", priority = 100, config = { syntax = "<syntax>" } }` unless the model profile declares an entry with that name, in which case the declared entry is used and its `priority` MUST be 100 (`E_PARSER_SLOT`) and `config.syntax`, if present, MUST equal `<syntax>`. When `tool_format = "native"`, an entry named `tool_call_parser` is rejected (`E_PARSER_SLOT`: no parser without a parsed format). Only the model profile may declare `tool_call_parser`; its priority (100) is outside the agent and project ranges, so those layers cannot reach it — this is the mechanical form of the "fixed slot".

**Ordering.** The resolved chain is the union of all layers' entries keyed by `name` (§6.3), sorted by a **stable sort on `priority`**; ties keep the order in which the entry was first declared, layer 0 first, then layer 1, 2, 3, in file order. **All six hooks run the chain head → tail in the same order** (kernel-interface.md fixes this; there is no onion/reverse order for the `after_*` hooks). It is repeated here because it is what makes 100 the right place for the tool-call parser (it must be the first `after_model` to see the raw response) and 990 the right place for the recorder (it must see final values in every hook).

### 2.8 Complete model profile example

The llama.cpp stand-in behind LiteLLM (D18, P0.5), as shipped at `profiles/models/stand-in.toml`. `id` is a LiteLLM route name so that the CI model can be swapped without touching any profile hash.

```toml
schema_version = 1

[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 32768
tool_format = "native"
temperature = 0.0
max_output_tokens = 2048

[model.thinking]
enabled = false
budget_tokens = 0

[model.quirks]
reasoning_field = "none"
supports_structured_output = false
supports_stream_usage = true
strict_tool_schema = false
auth = "bearer:LITELLM_CI_API_KEY"

[prompt]
text = """You are a coding and scripting assistant working inside a sandboxed checkout.
Use the tools. Do not narrate tool calls you could make. Keep answers short.
Long-running commands return a task id; you will be told when they finish."""

[tool_descriptions]
bash = "Run one shell command in a fresh sandbox. Shell state does not persist between calls; the filesystem does."

[compaction]
trigger_at = 0.80
target = 0.45
```

`auth` names a secret even though the CI proxy could run open, so that the D10 redaction tests (P1.3) exercise a real credential path in CI.

---

## 3. Agent profile schema

One file per role. All tables are optional except `[agent]` and `[tools]`.

### 3.1 `[agent]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `name` | string, `[a-z][a-z0-9_-]*` | yes | Must equal the catalog entry name that references this file (`E_CATALOG_REF`). |
| `description` | string | no | Shown in the UI's agent picker; not part of any prompt. |
| `role_prompt` | text source | no | Block 2 of §8. |
| `agents_md` | path | no (default `${workdir}/AGENTS.md`) | Block 3. A missing file omits the block and emits `W_AGENTS_MD_MISSING`; it is not an error, because a fresh repo has none. Read once at session start (§8.3). |
| `context_budget_tokens` | integer > 0 | no (default 40000, D16) | Per-turn context target; exceeding it is a warning event (P2.9). |
| `spill_cap_bytes` | integer in `[1024, 1048576]` | no (default 16384) | Tool results larger than this are spilled to `ArtifactStore` and replaced by `{ handle, head, tail }` by the kernel (D12). Only the size is a profile value; spilling itself cannot be disabled (§4.2). |

#### `[agent.eval_set]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `dev` | path or URL | yes if table present | Visible development set. Relative paths resolve against the profile file. |
| `hidden` | path or URL | yes if table present | Held-out set (D19). A URL (`git+ssh://…`, `https://…`) or an absolute path. It MUST NOT be under `${workdir}` and MUST NOT be covered by any `fs.*` grant of this profile after expansion (`E_HIDDEN_EVAL_REACHABLE`): the proposer's sandbox never mounts it, and this check is how P4.2's test is made static. Only the eval runner (P4.2) dereferences it. |

The table is absent from the P1 default agent and required for any profile registered as an evolve target (P4.2 enforces; the validator does not).

### 3.2 `[capabilities]`

```toml
[capabilities]
grants = ["fs.rw:${workdir}", "fs.ro:${home}/.cache/pip", "proc:bash", "proc:python3", "solver"]
```

| Key | Type | Meaning |
|---|---|---|
| `grants` | list of strings | Capability atoms in string form (§11) and/or bundle names (§5). Order is irrelevant; the list is normalized, bundle-expanded, deduplicated, and sorted before hashing. |

Two atom kinds are **derived, never written here**: `tool:<name>` comes from `[tools].allow` and `spawn:<catalog-name>` comes from `[[subagents]]`. Writing them in `grants` is rejected (`E_DERIVED_ATOM_IN_GRANTS`) so that there is one source of truth for each. The resolved grant set is:

```
grants_resolved = expand_bundles(capabilities.grants)
                ∪ { Tool{name}          | name in tools.allow }
                ∪ { Spawn{catalog_name} | entry in subagents }
```

### 3.3 `[tools]`

| Key | Type | Required | Meaning |
|---|---|---|---|
| `allow` | list of tool names | yes | The tools the kernel instantiates — enforcement level 1 of dev plan §6: the model cannot call what is not registered. Every name MUST be in `Registry.tools` (`E_TOOL_UNKNOWN`). Every capability the tool declares (`Tool::capabilities()`, concrete because tools are constructed with the session workdir) MUST be `narrower_than` some atom in `grants_resolved` (`E_CAP_EXCEEDS_GRANTS`). |

MCP tools (`mcp.<server>.<tool>`) and out-of-process extension tools (`ext.<manifest>.<tool>`, P2.1) are not listed here; they are admitted by their `[[mcp_servers]]` entry or manifest, and their derived `Tool{name}` atoms join `grants_resolved` at registration time (P2.2), which is when their names become known.

### 3.4 `[[mcp_servers]]` (P2.2)

| Key | Type | Required | Meaning |
|---|---|---|---|
| `name` | string, `[a-z][a-z0-9_-]*` | yes | Unique within the resolved list. Tools are exposed as `mcp.<name>.<tool>`. |
| `transport` | `"stdio"` or `"http"` | yes | |
| `command` | list of strings | when `stdio` | argv; element 0 is the program. Placeholders are expanded in every element. |
| `url` | string | when `http` | The host of the URL MUST be covered by a `net:` atom in `capabilities` below (`E_MCP_URL_HOST`). |
| `capabilities` | list of strings | yes | What the server process runs under. Each atom MUST be `narrower_than` some grant (`E_CAP_EXCEEDS_GRANTS`). For `stdio`, MUST include `proc:<command[0]>`. |
| `tools` | list of strings | no | Restrict to these server-side tool names; absent = all. |
| `lazy` | bool | no (default `true`) | `true`: schemas stay out of the prompt until `find_tools` or a skill names them. `false`: schemas are always in the prompt — allowed but counted against `context_budget_tokens`. |
| `env` | table of string → string | no | Extra environment for the server process; values MUST NOT contain secrets inline — use `secret:<NAME>` atoms in `capabilities` and the launcher injects the handle-resolved value under the same name. Keys are subject to the `env_allow` secret-pattern rule (§3.9). |

An MCP server is a `Session`-kind tool host (D5): one sandboxed process per session under `kernel::derive_policy_with(capabilities, grants_resolved, &limits)`.

### 3.5 `[skills]` (P2.3)

| Key | Type | Meaning |
|---|---|---|
| `paths` | list of directory paths | Searched in order for `*.md` skill files. Placeholders allowed. Directories that do not exist at session start emit `W_SKILL_PATH_MISSING`. |

Skill frontmatter is `name`, `description`, `tools` (names it references; exposing them for the turn), `capabilities` (atoms it needs). At load, each skill's `capabilities` MUST be `narrower_than` some grant, else the skill is refused with `E_CAP_EXCEEDS_GRANTS` and a `ProfileLoad{kind:"skill"}` event is still emitted with `rejected = true` so the trace shows what was tried. Accepted skills are hashed and logged as `ProfileLoad{kind:"skill"}`.

### 3.6 `[[subagents]]` (P2.4)

| Key | Type | Required | Meaning |
|---|---|---|---|
| `name` | string | yes | Local alias the parent uses in `spawn(name, task)`. Unique within the list. |
| `catalog` | string | yes | A catalog entry name (`E_CATALOG_REF` if absent from `catalog.toml`). |
| `capabilities` | list of strings | yes | The **ceiling** for the child. Every atom MUST be `narrower_than` some atom of the parent's `grants_resolved` (`E_CAP_EXCEEDS_GRANTS`). Bundle names allowed. |
| `definition` | path | no | dcode-style markdown-with-frontmatter supplying the task-prompt body; its frontmatter `catalog`, if present, MUST equal `catalog`. |
| `description` | string | no | |

Monotone narrowing (D6, dev plan §6 level 3) is checked twice: at **resolution** (statically, using the child's catalog profiles with layers 0–2 only) and at **spawn** (P2.4, with the child's full resolution including its own project overrides). At both points every atom of the child's `grants_resolved` — including its derived `Tool` and `Spawn` atoms — MUST be `narrower_than` some atom in the ceiling, and every ceiling atom `narrower_than` some parent grant. Consequence of D6 taken literally: **a parent must itself hold `tool:<name>` (i.e. list the tool in `[tools].allow`) for every tool any of its sub-agents uses**, even if the parent never calls it. See open question 3.

Recursion is permitted (an orchestrator may declare an orchestrator); the static check memoizes on catalog name and terminates.

### 3.7 `[[middleware]]`

As §2.7, priority range 200–899.

### 3.8 `[memory]` (P2.7)

| Key | Type | Default | Meaning |
|---|---|---|---|
| `module` | string | `"none"` | MUST be in `Registry.memory_modules`. `"none"` selects the no-op `Memory` implementation (D12). |
| `config` | table | `{}` | Opaque to the validator; validated by the module. |

### 3.9 `[sandbox]`

The sandbox policy is **derived mechanically from atoms** by `kernel::derive_policy_with(caps, grants, &SandboxLimits)` (pure, defined in `kernel`, re-exported by `sandbox`; D5, P1.7); this table supplies limits and can only **narrow** what the atoms would otherwise allow. It can never grant anything.

| Key | Type | Default | Meaning and narrowing rule |
|---|---|---|---|
| `backend` | string | `"bwrap"` | MUST be in `Registry.sandbox_backends`. `"none"` is accepted **only** when `Registry.dev_build` is true (the `dev-sandbox-none` feature, D14); otherwise `E_SANDBOX_NONE_FORBIDDEN`. Every session under `"none"` logs it (`session_created.sandbox_backend` plus a `warning{class: "sandbox_backend_none"}`, event-schema.md). Not overridable in layer 3. |
| `timeout_s` | integer > 0 | `600` | Wall-clock limit per tool invocation (Stateless) or per RPC call (Session). Layer 3 may only lower it. |
| `scratch_tmpfs_mb` | integer > 0 | `256` | Size of the tmpfs mounted at `/tmp` inside the sandbox. Layer 3 may only lower it. |
| `env_allow` | list of strings | `["PATH","HOME","LANG","LC_ALL","TERM","TZ"]` | Environment variables passed through from the kernel process. Everything else is scrubbed (D10). A name matching `(?i)(KEY|TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL)` is rejected outright (`E_ENV_ALLOW_SECRET_PATTERN`), in every layer. Layer 3 may only remove names. |
| `network` | bool | `false` | Master switch. `false` masks every `net:` atom when the policy is derived (the atoms remain in `grants_resolved` and in the hash; the sandbox gets no network), with `W_NET_MASKED` if any `net:` atom was present. `true` lets the atoms decide: still no network unless some `net:` atom is granted. Layer 3 may only set it to `false`. |

### 3.10 `[notebook]` (P2.6)

| Key | Type | Default | Meaning |
|---|---|---|---|
| `path` | path | `${workdir}/.grist/notebook.md` | MUST be covered by an `fs.rw` grant after expansion (`E_CAP_EXCEEDS_GRANTS`, naming the atom `fs.rw:<path>`), since the harness writes it. |
| `inject_on_resume` | bool | `true` | Block 5 of §8. |
| `max_tokens` | integer > 0 | `4000` | Compaction keeps the notebook under this; injection truncates from the top with a marker if it is over. |

### 3.11 Complete agent profile example

The default agent shipped at `profiles/agents/default.toml` (P1.8): the four base tools, `run_script`, and the Python REPL (P1.7).

```toml
schema_version = 1

[agent]
name = "default"
description = "General-purpose coding and scripting agent for a single checkout."
context_budget_tokens = 40000
spill_cap_bytes = 16384
agents_md = "${workdir}/AGENTS.md"

[agent.role_prompt]
file = "default.role.md"

[capabilities]
grants = ["fs.rw:${workdir}", "proc:bash", "proc:python3"]

[tools]
allow = ["read", "write", "edit", "bash", "run_script", "python"]

[skills]
paths = ["${workdir}/.grist/skills"]

[memory]
module = "none"

[sandbox]
backend = "bwrap"
timeout_s = 600
scratch_tmpfs_mb = 256
env_allow = ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"]
network = false

[notebook]
path = "${workdir}/.grist/notebook.md"
inject_on_resume = true
max_tokens = 4000
```

with `profiles/agents/default.role.md`:

```markdown
You work in the repository mounted at the working directory. Read before you edit.
Run tests with `bash`; use `run_script` for anything that takes longer than a minute
and continue with other work until its result arrives. Use `python` for data and
numerics; its state persists across calls within a session.
```

The tools' declared capabilities (P1.7) and why the grants above suffice:

| Tool | Kind (D5) | Declares | Covered by |
|---|---|---|---|
| `read` | in-process (Host policy) | `fs.ro:${workdir}` | `fs.rw:${workdir}` (Ro ≤ Rw) |
| `write`, `edit` | in-process (Host policy) | `fs.rw:${workdir}` | `fs.rw:${workdir}` |
| `bash` | Stateless | `fs.rw:${workdir}`, `proc:bash` | both |
| `run_script` | Stateless, returns `Task` (D1) | `fs.rw:${workdir}`, `proc:bash` | both |
| `python` | Session | `fs.rw:${workdir}`, `proc:python3` | both |

No `net:` atom, so the default agent has no network (dev plan §8).

---

## 4. Project overrides

### 4.1 File and shape

`<workdir>/.grist/agent.toml`, same `schema_version`, same table names as the agent and model profiles. It is layer 3 and — because it lives inside the `fs.rw:${workdir}` grant — **agent-writable**, so it can only tune and narrow.

```toml
schema_version = 1

[agent]
context_budget_tokens = 30000

[capabilities]
grants = ["fs.rw:${workdir}", "proc:bash"]      # narrower than the base: python dropped

[tools]
allow = ["read", "write", "edit", "bash", "run_script"]

[sandbox]
timeout_s = 120

[[middleware]]
name = "compaction"
priority = 300
config = { style = "notebook" }

[compaction]
trigger_at = 0.70
target = 0.40
```

### 4.2 What layer 3 may and may not touch

| Table / key | Layer 3 | Rule |
|---|---|---|
| `agent.context_budget_tokens`, `agent.spill_cap_bytes`, `agent.agents_md`, `agent.role_prompt`, `agent.description` | yes | values |
| `agent.name`, `agent.eval_set` | **no** | identity and eval pointers are not steerable from the workdir (D19, P4.4) → `E_OVERRIDE_FORBIDDEN_KEY` |
| `capabilities.grants` | narrowing only | the list replaces (§6.1) and every atom of the new resolved set MUST be `narrower_than` some atom of the layer-2 resolved set → else `E_OVERRIDE_WIDENS_GRANTS` |
| `tools.allow` | subset only | derived `tool:` atoms fall under the rule above; the error names the tool |
| `mcp_servers`, `subagents`, `skills.paths` | yes | lists replace; each entry is bounded by the (possibly narrowed) grants, so nothing here can widen the sandbox |
| `middleware` | add or override, never remove | entries merge by name (§6.3); priorities MUST stay in 200–899; an entry whose name resolves to a model-profile or kernel slot → `E_PRIORITY_RANGE` |
| `memory`, `notebook` | yes | values; `notebook.path` still needs an `fs.rw` grant |
| `sandbox.timeout_s`, `sandbox.scratch_tmpfs_mb` | lower only | `E_OVERRIDE_WIDENS_SANDBOX` |
| `sandbox.env_allow` | remove only | `E_OVERRIDE_WIDENS_SANDBOX` |
| `sandbox.network` | `false` only | `E_OVERRIDE_WIDENS_SANDBOX` |
| `sandbox.backend` | **no** | `E_OVERRIDE_FORBIDDEN_KEY` |
| `model.temperature`, `model.max_output_tokens`, `model.thinking.*` | yes | sampling values |
| `compaction.*`, `tool_descriptions.*` | yes | values; a project may phrase tools for its domain |
| `model.id`, `model.endpoint`, `model.context_length`, `model.tool_format`, `model.quirks.*`, `prompt`, `fine_tune` | **no** | model identity and its prompt variant belong to the model axis → `E_OVERRIDE_FORBIDDEN_KEY` |

### 4.3 Kernel-only keys, rejected in every file

These name kernel behaviours (D12: "a profile cannot disable it"). They are rejected with `E_KERNEL_ONLY_KEY` rather than `E_UNKNOWN_KEY` so that an author — or a proposer agent — learns it is a boundary, not a typo:

| Top-level table or key | Why |
|---|---|
| `loop` | the §4.1 loop is code |
| `checkpoint` | checkpoint-per-turn is a kernel invariant |
| `spill` (any key, including `spill.enabled`) | spill lives in the kernel; only `agent.spill_cap_bytes` is a value |
| `event_log`, `hashing`, `state` | log format, envelope, hash algorithm, state schema |
| `record`, `replay` | recorder / replay are kernel-inserted middleware |
| `kernel`, `protocol`, `orchestrator`, `provenance`, `outer_sandbox` | out of the mutable layer entirely (dev plan §8: the outer sandbox is never evolved) |
| `sandbox.backend = "none"` outside a dev build | D14 → `E_SANDBOX_NONE_FORBIDDEN` (its own code, since the key is otherwise legal) |
| `sandbox.outer`, `sandbox.seccomp`, `sandbox.uidmap` | outer sandbox and launcher concerns (D18) |

### 4.4 Runtime overrides (layer 4)

The protocol start-session message may carry `profile_overrides`, a flat map whose keys MUST be from this whitelist; any other key fails the start-session request with `E_OVERRIDE_FORBIDDEN_KEY`:

`model.temperature`, `model.max_output_tokens`, `model.thinking.enabled`, `model.thinking.budget_tokens`, `agent.context_budget_tokens`, `notebook.path`.

They merge as a layer with the §6 rules and are therefore inside `resolved_profile_hash`. There is no `ProfileLoad` event for them (they are not a file); they appear in `session_created.overrides` (event-schema.md §2.2).

---

## 5. Bundles file

`profiles/bundles.toml`, exactly one, in-repo. Bundles are named atom sets (D6); the kernel and sandbox never see a bundle name.

### 5.1 Schema

```
schema_version = 1
[install]                 # table: tool name → absolute install path (D9 placeholders)
[bundles.<name>]          # <name> = [a-z][a-z0-9_-]*, not a reserved prefix (§11.1)
description = "<string>"  # optional
atoms = ["<atom-string>", ...]   # atoms only: a bundle MUST NOT name another bundle
```

- `atoms` may contain `${workdir}`, `${home}`, and `${install:<tool>}` placeholders; `${install:<tool>}` MUST name a key of `[install]` (`E_UNKNOWN_PLACEHOLDER`).
- `[install]` values MUST be absolute paths. They are the *only* source of install paths; there is no environment override, so the P0.5 mock solver is installed at exactly the placeholder path inside the CI container (D9, D18). Changing an install path is a change to `bundles_hash`, which is what we want provenance to see.
- Bundles are flat (no nesting) so expansion is a single lookup.

### 5.2 Shipped content

```toml
schema_version = 1

# In-house tools are placeholders (D9). CI installs the mock solver at these
# exact paths (P0.5). Real paths land in the same PR as the tool interfaces.
[install]
mesher = "/opt/inhouse/mesher"
solver = "/opt/inhouse/solver"
post = "/opt/inhouse/post"

# D6 / D9: an in-house HPC tool is granted as its install tree read-only,
# the Slurm submit path, and the working directory read-write.

[bundles.meshing]
description = "Generate and inspect meshes with the in-house mesher via Slurm."
atoms = [
  "fs.ro:${install:mesher}",
  "proc:sbatch",
  "proc:squeue",
  "proc:scancel",
  "fs.rw:${workdir}",
]

[bundles.solver]
description = "Set up, submit, and monitor in-house solver runs via Slurm."
atoms = [
  "fs.ro:${install:solver}",
  "proc:sbatch",
  "proc:squeue",
  "proc:scancel",
  "fs.rw:${workdir}",
]

[bundles.post]
description = "Post-process results with the in-house post tool; visualization to PNG."
atoms = [
  "fs.ro:${install:post}",
  "proc:sbatch",
  "fs.rw:${workdir}",
]
```

`squeue`/`scancel` are in `meshing` and `solver` because the P3.4 Slurm wrapper needs them to poll and cancel; `post` runs to completion and needs only `sbatch`. Whether `proc:sbatch` also needs a `net:` atom on real Slurm is open question 5.

### 5.3 Expansion

`expand_bundles(list) -> Result<Vec<String>>`: for each string, if it has a `<prefix>:` form (§11.1) it is an atom and passes through; otherwise it MUST be a `[bundles.<name>]` key (`E_UNKNOWN_BUNDLE`) and is replaced by that bundle's `atoms`. The result keeps placeholders symbolic (§7.6) and is deduplicated and sorted lexically by string form.

---

## 6. Merge rules (D7)

### 6.1 Value rules

Applied recursively, later layer over earlier layer:

| Earlier | Later | Result |
|---|---|---|
| scalar | scalar | later (scalars override) |
| table | table | deep-merge, key by key (tables deep-merge) |
| list | list | later, wholesale (lists replace, never append) |
| any | absent | earlier (absent means inherit) |
| shape A | shape B (scalar vs table vs list) | later, wholesale (type mismatch) |

There is no `null` in TOML and no "unset" operation: to restore a default, write the default value. Array-of-tables (`[[x]]`) are lists and **replace**, with one exception, `[[middleware]]` (§6.3), which D7 names explicitly.

### 6.2 Worked example

Layer 0 (kernel defaults, excerpt), layer 1 (model), layer 2 (agent), layer 3 (project):

```toml
# layer 1 — profiles/models/stand-in.toml (excerpt)
[model]
temperature = 0.0
max_output_tokens = 2048
[compaction]
trigger_at = 0.80
target = 0.45
[[middleware]]
name = "thinking_strip"
priority = 150
```

```toml
# layer 2 — profiles/agents/default.toml (excerpt)
[sandbox]
timeout_s = 600
env_allow = ["PATH", "HOME", "LANG", "LC_ALL", "TERM", "TZ"]
[[middleware]]
name = "compaction"
priority = 300
config = { style = "paragraph", keep_last = 6 }
[[middleware]]
name = "budget"
priority = 800
```

```toml
# layer 3 — <workdir>/.grist/agent.toml
[model]
temperature = 0.3
[sandbox]
timeout_s = 120
env_allow = ["PATH", "LANG"]
[[middleware]]
name = "compaction"
priority = 250
config = { style = "notebook" }
```

Resolved (excerpt, shown as TOML; the real output is the `ResolvedProfile` struct of §7.6):

```toml
[model]
temperature = 0.3            # scalar: layer 3 over layer 1
max_output_tokens = 2048     # inherited from layer 1
[compaction]
trigger_at = 0.80            # layer 1 over layer 0 (0.85)
target = 0.45
[sandbox]
timeout_s = 120              # narrowed by layer 3 (allowed: lower)
env_allow = ["PATH", "LANG"] # list replaced (allowed: subset)
scratch_tmpfs_mb = 256       # layer 0
network = false              # layer 0
```

and the middleware chain, after keyed merge and stable sort:

| priority | name | source of entry | config | note |
|---|---|---|---|---|
| 150 | `thinking_strip` | model | `{}` | model-slot range |
| 250 | `compaction` | agent, overridden by project | `{ style = "notebook" }` | `keep_last` is **gone**: `config` is a table but the override replaces the *entry's* `priority` and `config` per §6.3, not key-by-key |
| 800 | `budget` | agent | `{}` | |
| 990 | `recorder` | kernel | `{}` | inserted from code |

### 6.3 Middleware chain merge

Entries are keyed by `name`. Within one file a duplicate name is rejected (`E_DUP_MIDDLEWARE`). Across layers:

- a later layer with the same `name` **replaces that entry's `priority` and `config`** (the `config` table is replaced, not deep-merged — a middleware's config is one value from the profile's point of view; this is the one deliberate departure from "tables deep-merge", chosen so an override cannot leave a half-merged config the middleware never anticipated);
- a later layer **cannot delete** an entry — there is no removal syntax, and a profile is not allowed to disable middleware a lower layer installed (dev plan §14, "middleware removal");
- the kernel's entries are added last and cannot be named by any file;
- the resolved chain is the stable sort of §2.7 and is emitted at run start as the `MiddlewareChainResolved` event with payload `{ chain: [{ name, priority, source: kernel|model|agent|project, config_hash }] }` (event-schema.md), which P1.2 writes into every log.

---

## 7. Resolution algorithm

`profiles::resolve(inputs) -> Result<Resolved, Vec<Diagnostic>>` runs the following steps in order and stops at the first step that produces an error; all errors of that step are reported together.

### 7.1 Inputs

```rust
pub struct ResolveInputs<'a> {
    pub profiles_dir: &'a Path,           // holds catalog.toml, bundles.toml
    pub workdir: &'a Path,                // absolute
    pub home: &'a Path,                   // absolute
    pub agent: &'a str,                   // catalog entry name
    pub runtime_overrides: &'a toml::Table,
    pub registry: &'a Registry,
    pub resume: bool,                     // §8: notebook block
}

/// What the validator must know about this build. Supplied by the kernel
/// binary that assembles tools and middleware; `profiles` cannot depend on
/// `ext` (crates/README.md), so nothing here is discovered by the crate itself.
pub struct Registry {
    pub tools: BTreeMap<String, ToolDecl>,          // name -> { kind, capabilities }
    pub middleware: BTreeSet<String>,
    pub parsers: BTreeSet<String>,                  // tool-call syntaxes with a compiled parser
    pub memory_modules: BTreeSet<String>,
    pub sandbox_backends: BTreeSet<String>,
    pub dev_build: bool,                            // `dev-sandbox-none` feature on (D14)
}
pub struct ToolDecl { pub kind: kernel::ToolKind, pub capabilities: Vec<kernel::Capability> }
```

### 7.2 Steps

1. **Load** `catalog.toml`, `bundles.toml`, the named entry's model and agent profile files, `<workdir>/.grist/agent.toml` if present. Read each as bytes; compute `b3:` over the raw bytes (this is the *source file hash* in `ProfileLoad.hash`; it is a hash of bytes, not of canonical JSON, so that a byte-for-byte edit is always visible).
2. **Parse and validate each file against its own schema**, independently: `schema_version` (§12), unknown keys at every level (`E_UNKNOWN_KEY`), kernel-only keys (`E_KERNEL_ONLY_KEY`), types and ranges (`E_VALUE_RANGE`, `E_MISSING_KEY`), text sources (`E_TEXT_SOURCE`), atom grammar (`E_MALFORMED_ATOM`), duplicate middleware names within the file (`E_DUP_MIDDLEWARE`), priority range for that file kind (`E_PRIORITY_RANGE`), reserved names, the layer-3 forbidden-key table (§4.2), `env_allow` secret patterns. The bundles file and catalog are validated here too.
3. **Merge** layers 0 → 1 → 2 → 3 → 4 per §6, including the keyed middleware merge. Then check the required keys are present.
4. **Expand bundles** in `capabilities.grants`, every `mcp_servers[].capabilities`, and every `subagents[].capabilities` (§5.3). Add derived `tool:` and `spawn:` atoms (§3.2). Placeholders stay symbolic.
5. **Synthesize the parser entry** if `tool_format = "parsed:<syntax>"` (§2.7); check `<syntax>` ∈ `Registry.parsers` (`E_PARSER_UNAVAILABLE`). Check every middleware name ∈ `Registry.middleware` (`E_MIDDLEWARE_UNKNOWN`), memory module, sandbox backend (`E_SANDBOX_NONE_FORBIDDEN` when applicable). Stable-sort the chain.
6. **Compute `resolved_profile_hash`** over the canonical JSON of the `ResolvedProfile` struct (§7.6) — *before* placeholder expansion, so the hash is portable across machines and workdirs.
7. **Expand placeholders** (§7.4) in every path-typed field and every atom string; **normalize** (§11.3). Reject paths that are not absolute after expansion (`E_PATH_NOT_ABSOLUTE`) or contain `..` (`E_PATH_DOTDOT`). Parse each atom string into `kernel::Capability`.
8. **Narrowing checks** using `kernel::Capability::narrower_than` (§11.4), each failure reported as `E_CAP_EXCEEDS_GRANTS` naming the offending atom in string form and its owner:
   - every atom in `Registry.tools[name].capabilities` for each `tools.allow` name;
   - every atom of each MCP server, plus the `E_MCP_URL_HOST` check;
   - every atom of each sub-agent ceiling; then, resolving each referenced catalog entry with layers 0–2, every atom of the child's `grants_resolved` against the ceiling;
   - `notebook.path` as `fs.rw`;
   - `agent.eval_set.hidden` must **not** be covered (`E_HIDDEN_EVAL_REACHABLE`);
   - layer 3 narrowing rules (`E_OVERRIDE_WIDENS_GRANTS`, `E_OVERRIDE_WIDENS_SANDBOX`) by comparing the layer-2-only resolution against the full one.
9. **Derive the sandbox envelope**: `kernel::derive_policy_with(&grants_resolved, &grants_resolved, &limits) -> Result<kernel::SandboxPolicy, PolicyError>`, where `limits` is the `[sandbox]` table (`network = false` masks `Net` atoms first). Per-tool policies are derived by the launcher at call time from `(tool.capabilities, grants_resolved, limits)` and are by construction narrower than the envelope. `sandbox_policy_hash = b3(canonical_json(envelope))`.
10. **Assemble the system prompt** blocks (§8) for the first turn; load `AGENTS.md` and, if `resume`, the notebook.
11. **Emit events** in this order: `ProfileLoad{kind:"bundles"}`, `ProfileLoad{kind:"model"}`, `ProfileLoad{kind:"agent"}`, `ProfileLoad{kind:"project"}` (only if the file exists), then `middleware_chain_resolved`, then `warning{class: "profile_drift"}` if §2.6 applies. Skill `ProfileLoad` events are emitted per turn as skills activate (P2.3).
12. **Record in `State`**: `State.profiles: kernel::ActiveProfiles { model_profile_hash, agent_profile_hash, resolved_profile_hash, project_profile_hash: Option<Hash>, bundles_hash }`, `State.sandbox_policy_hash`, `State.sandbox_backend`, `State.notebook_path`. `model_request` events copy `State.profiles` into their D13 fields.

### 7.3 Diagnostics

```rust
pub struct Diagnostic {
    pub code: &'static str,        // "E_UNKNOWN_KEY", "W_NET_MASKED", ...
    pub file: Option<PathBuf>,     // None for merge-time diagnostics
    pub toml_path: String,         // "agent.context_budget_tokens", "middleware[2].priority", "capabilities.grants[3]"
    pub message: String,
}
impl Display for Diagnostic  // "<code> at <file>:<toml_path>: <message>"
```

Codes starting `E_` are errors (resolution fails; the session does not start); `W_` are warnings (logged as `warning{class: "profile_warning", detail: {code, toml_path}, source: "profiles"}`, event-schema.md §2.28). The tests in §9 assert on the `Display` prefix `<code> at <file-basename>:<toml_path>`.

### 7.4 Placeholders

Grammar: `"${" name [":" arg] "}"` with `name ∈ { workdir, home, install }`; `install` requires `arg` = a key of `bundles.toml [install]`; the others take none. Anything else is `E_UNKNOWN_PLACEHOLDER`.

| Placeholder | Expands to | Source |
|---|---|---|
| `${workdir}` | the session's working directory, absolute, symlinks **not** resolved | `ResolveInputs.workdir` (protocol start-session) |
| `${home}` | the kernel process user's home directory | `ResolveInputs.home` (the launcher; never `$HOME` read by `profiles` itself, so replay is hermetic) |
| `${install:<tool>}` | `bundles.toml [install].<tool>` | bundles file |

Placeholders are expanded **only** in path-typed fields (`agents_md`, `skills.paths[]`, `notebook.path`, `subagents[].definition`, `mcp_servers[].command[]`, `eval_set.*`) and in capability atom strings, and only at the **start** of the string (`${workdir}/sub` is valid; `/x/${workdir}` is `E_MALFORMED_ATOM` / `E_PATH_NOT_ABSOLUTE`). They are never expanded in prose (§2.3) or in `config` tables (a middleware that wants the workdir asks the kernel).

### 7.5 What is checked per layer versus after merge

| Check | When | Why |
|---|---|---|
| unknown / kernel-only keys, grammar, ranges, per-file priority range | per file (step 2) | the error names the file and line; a layer cannot hide a typo in another |
| required keys | after merge (step 3) | a model profile may legitimately omit `[prompt]`; `[model].id` may not be omitted by *both* layers |
| capability narrowing, override narrowing | after expansion (steps 7–8) | needs concrete paths |

### 7.6 The resolved struct and its hash

`resolved_profile_hash = b3(canonical_json(ResolvedProfile))` with this shape (serde field names are the JSON keys; RFC 8785 sorts them, so declaration order is not significant, but names and types are load-bearing and MUST NOT change without a `schema_version` bump):

```rust
#[derive(Serialize)]
pub struct ResolvedProfile {
    pub schema_version: u32,
    pub catalog_entry: String,
    pub model: ModelSection,           // id, endpoint, context_length, tool_format, temperature: Option<f64>,
                                       // max_output_tokens, thinking { enabled, budget_tokens }, quirks { ... }
    pub prompt: Option<String>,        // text source *resolved to text*
    pub tool_descriptions: BTreeMap<String, String>,   // resolved to text
    pub compaction: Compaction,        // trigger_at, target
    // fine_tune is deliberately NOT here (§2.6)
    pub agent: AgentSection,           // name, description, role_prompt: Option<String> (text), agents_md (symbolic path),
                                       // context_budget_tokens, spill_cap_bytes, eval_set: Option<{dev, hidden}>
    pub grants: Vec<String>,           // bundle-expanded, derived atoms included, symbolic, normalized, sorted, deduped
    pub tools: Vec<String>,            // sorted
    pub mcp_servers: Vec<McpServer>,   // in file order; capabilities expanded+sorted
    pub skills_paths: Vec<String>,     // symbolic
    pub subagents: Vec<Subagent>,      // in file order; ceiling expanded+sorted
    pub middleware: Vec<MiddlewareEntry>,  // resolved chain in sorted order, kernel entries included:
                                           // { name, priority, source, config: serde_json::Value }
    pub memory: Memory,                // module, config
    pub sandbox: SandboxLimits,        // backend, timeout_s, scratch_tmpfs_mb, env_allow (sorted), network
    pub notebook: Notebook,            // path (symbolic), inject_on_resume, max_tokens
    pub runtime_overrides: BTreeMap<String, serde_json::Value>,   // layer 4, verbatim
}
```

Floats are serialized per RFC 8785 (shortest round-trip). Text sources are resolved to their text so that editing `default.role.md` changes the hash. Paths are symbolic (`${workdir}/…`) so that the same profile in two checkouts hashes identically; the concrete binding is in `sandbox_policy_hash`.

---

## 8. System prompt assembly (D7)

### 8.1 Blocks, in fixed order

| # | Block | Source | Header line | Varies |
|---|---|---|---|---|
| 1 | model prompt variant | `[prompt]` | none | per session |
| 2 | agent role prompt | `[agent].role_prompt` | none | per session |
| 3 | project instructions | `[agent].agents_md` file | `# Project instructions (AGENTS.md)` | per session |
| 4 | active skills | skills activated for this turn (P2.3), each `## Skill: <name>` then its body, in activation order | `# Active skills` | **per turn** |
| 5 | notebook | `[notebook].path` contents | `# Notebook` | **per turn**: present on the first model call after `on_resume` when `inject_on_resume = true`; P2.6 may additionally present it after a compaction |

### 8.2 Joining

- A block's text is trimmed of leading and trailing newlines, then prefixed with its header line (blocks 3–5) followed by one blank line.
- Empty blocks (absent table, missing `AGENTS.md`, no active skills, no notebook) are **omitted entirely** — no header, no separator.
- Non-empty blocks are joined with the separator `"\n\n"`.
- If every block is empty, no system message is sent.
- No other text is added: no date, no tool list (tool schemas travel in the request's tool field, not the prompt), no environment description. Anything else a team wants is a role prompt or `AGENTS.md`.

### 8.3 Per-session versus per-turn

Blocks 1–3 are read once, at session start (and again at resume, since a resumed process rebuilds them), and held for the session; editing `AGENTS.md` mid-session does not change the prompt until the next resume. This keeps a session's model calls reproducible from its checkpoint (D16). Blocks 4–5 are recomputed every turn from state the kernel already logs (skill activations, notebook path), so replay reproduces them.

### 8.4 Hashing

The assembled system prompt string is part of the request body and is therefore covered by `request_hash` as event-schema.md defines it (the hash of the canonical request: model id, system prompt, messages, tool schemas, sampling parameters). In addition, every `model_request` event carries `prompt_blocks: [{ kind: model|role|agents_md|skills|notebook, hash }]`, where `hash` is `b3` of that block's UTF-8 bytes *after* header prefixing. This is what lets the provenance projector (P3.1) say "this call used role prompt X and skills Y, Z" without re-parsing the prompt.

---

## 9. Validator rejection cases

Each case is one test in P1.8 (`crates/profiles/tests/validator.rs`), named `rejects_<nn>_<slug>`. The snippet is the smallest file that fails; "expected" is the `Display` prefix of the first diagnostic (§7.3). Where a case needs a second file or a registry state, it is stated. Every snippet below is valid TOML — the failures are semantic.

**1a. Unknown key, top level** — model profile

```toml
schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 32768
[modle]
temperature = 0.1
```
Expected: `E_UNKNOWN_KEY at stand-in.toml:modle`

**1b. Unknown key, table level** — agent profile

```toml
schema_version = 1
[agent]
name = "default"
contxt_budget_tokens = 40000
[tools]
allow = ["read"]
```
Expected: `E_UNKNOWN_KEY at default.toml:agent.contxt_budget_tokens`

**1c. Unknown key, array-of-tables level** — agent profile

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "compaction"
priority = 300
prio = 300
```
Expected: `E_UNKNOWN_KEY at default.toml:middleware[0].prio`

**2. Kernel-only key**

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[spill]
enabled = false
```
Expected: `E_KERNEL_ONLY_KEY at default.toml:spill`

**3. Tool capability exceeds grants** — registry: `write` declares `fs.rw:<workdir>`

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.ro:${workdir}"]
[tools]
allow = ["write"]
```
Expected: `E_CAP_EXCEEDS_GRANTS at default.toml:tools.allow[0]: tool 'write' requires 'fs.rw:${workdir}'`

**4. MCP server capability exceeds grants**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "proc:npx"]
[tools]
allow = ["read"]
[[mcp_servers]]
name = "docs"
transport = "stdio"
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/srv/docs"]
capabilities = ["proc:npx", "fs.ro:/srv/docs"]
```
Expected: `E_CAP_EXCEEDS_GRANTS at default.toml:mcp_servers[0].capabilities[1]: 'fs.ro:/srv/docs'`

**5. Sub-agent ceiling exceeds parent grants** — catalog has an entry `debugger`

```toml
schema_version = 1
[agent]
name = "orchestrator"
[capabilities]
grants = ["fs.rw:${workdir}", "solver"]
[tools]
allow = ["read"]
[[subagents]]
name = "dbg"
catalog = "debugger"
capabilities = ["solver", "net:*"]
```
Expected: `E_CAP_EXCEEDS_GRANTS at orchestrator.toml:subagents[0].capabilities[1]: 'net:*'`

**6. Unknown bundle name**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "meshing", "thermal"]
[tools]
allow = ["read"]
```
Expected: `E_UNKNOWN_BUNDLE at default.toml:capabilities.grants[2]: 'thermal'`

**7. Malformed atom string**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rwx:${workdir}"]
[tools]
allow = ["read"]
```
Expected: `E_MALFORMED_ATOM at default.toml:capabilities.grants[0]: 'fs.rwx:${workdir}'`

(Also covered by the same test with `"net:"`, `"proc:"`, `"fs.ro:"` — empty operands — and `"Fs.ro:/x"` — wrong case.)

**8. Path not absolute after placeholder expansion**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "fs.ro:data/meshes"]
[tools]
allow = ["read"]
```
Expected: `E_PATH_NOT_ABSOLUTE at default.toml:capabilities.grants[1]: 'data/meshes'`

**9. `..` in path**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}/../shared"]
[tools]
allow = ["read"]
```
Expected: `E_PATH_DOTDOT at default.toml:capabilities.grants[0]`

**10. Duplicate middleware name in one file**

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "compaction"
priority = 300
[[middleware]]
name = "compaction"
priority = 310
```
Expected: `E_DUP_MIDDLEWARE at default.toml:middleware[1].name: 'compaction'`

**11a. Priority outside the range for the file kind** — agent profile using a model slot

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "compaction"
priority = 150
```
Expected: `E_PRIORITY_RANGE at default.toml:middleware[0].priority: 150 not in 200..=899`

**11b.** — model profile using an agent slot

```toml
schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 32768
[[middleware]]
name = "thinking_strip"
priority = 300
```
Expected: `E_PRIORITY_RANGE at stand-in.toml:middleware[0].priority: 300 not in 100..=199`

**11c.** — project override using a kernel slot

```toml
schema_version = 1
[[middleware]]
name = "budget"
priority = 950
```
Expected: `E_PRIORITY_RANGE at agent.toml:middleware[0].priority: 950 not in 200..=899`

**12. `parsed:<syntax>` without the parser available** — registry: `parsers = {}`

```toml
schema_version = 1
[model]
id = "ft/qwen3-32b-2026-08"
endpoint = "vllm-ft"
context_length = 65536
tool_format = "parsed:hermes"
```
Expected: `E_PARSER_UNAVAILABLE at ft-qwen3.toml:model.tool_format: no parser for 'hermes'`

**13. `context_budget_tokens` ≤ 0**

```toml
schema_version = 1
[agent]
name = "default"
context_budget_tokens = 0
[tools]
allow = ["read"]
```
Expected: `E_VALUE_RANGE at default.toml:agent.context_budget_tokens: must be > 0`

(The same test covers `spill_cap_bytes = 0`; values above 1 MiB are not a validator error but are clamped by the kernel with `warning{class: "spill_cap_clamped"}`, kernel-interface.md §7.4; `sandbox.timeout_s = 0`, `model.context_length = 0`, `compaction.target = 0.9` with `trigger_at = 0.8`, `model.temperature = 3.0`.)

**14. Hidden eval set under the workdir**

```toml
schema_version = 1
[agent]
name = "default"
[agent.eval_set]
dev = "evals/dev"
hidden = "${workdir}/evals/hidden"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
```
Expected: `E_HIDDEN_EVAL_REACHABLE at default.toml:agent.eval_set.hidden`

(The same test asserts the second form: `hidden = "/data/evals/hidden"` with `grants = ["fs.ro:/data"]`.)

**15. Project override widening grants** — base agent `grants = ["fs.rw:${workdir}"]`

```toml
schema_version = 1
[capabilities]
grants = ["fs.rw:${workdir}", "net:*"]
```
Expected: `E_OVERRIDE_WIDENS_GRANTS at agent.toml:capabilities.grants[1]: 'net:*'`

(Same test: `[tools] allow = ["read", "python"]` where the base allows only `read` → `… tools.allow[1]: 'tool:python'`.)

**16. `sandbox.backend = "none"` in a non-dev build** — registry: `dev_build = false`

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[sandbox]
backend = "none"
```
Expected: `E_SANDBOX_NONE_FORBIDDEN at default.toml:sandbox.backend`

**17. Project override touching a forbidden key**

```toml
schema_version = 1
[agent]
name = "something-else"
```
Expected: `E_OVERRIDE_FORBIDDEN_KEY at agent.toml:agent.name`

(Same test: `[model] id = "x"`, `[agent.eval_set] hidden = "/x"`, `[sandbox] backend = "bwrap"`, `[fine_tune] trained_against_profile_hash = "b3:00…"`.)

**18. Project override widening the sandbox** — base `timeout_s = 600`, `network = false`, `env_allow = ["PATH"]`

```toml
schema_version = 1
[sandbox]
timeout_s = 3600
```
Expected: `E_OVERRIDE_WIDENS_SANDBOX at agent.toml:sandbox.timeout_s: 3600 > 600`

(Same test: `network = true`; `env_allow = ["PATH", "HOME"]`; `scratch_tmpfs_mb = 4096`.)

**19. Unknown middleware name** — registry lacks `foo`

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "foo"
priority = 300
```
Expected: `E_MIDDLEWARE_UNKNOWN at default.toml:middleware[0].name: 'foo'`

**20. Unknown tool**

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read", "browse"]
```
Expected: `E_TOOL_UNKNOWN at default.toml:tools.allow[1]: 'browse'`

(Same test for `[tool_descriptions] browse = "…"` in a model profile.)

**21. `env_allow` names a secret-shaped variable**

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[sandbox]
env_allow = ["PATH", "OPENAI_API_KEY"]
```
Expected: `E_ENV_ALLOW_SECRET_PATTERN at default.toml:sandbox.env_allow[1]: 'OPENAI_API_KEY'`

**22. Schema version missing or newer**

```toml
schema_version = 2
[agent]
name = "default"
[tools]
allow = ["read"]
```
Expected: `E_SCHEMA_VERSION at default.toml:schema_version: 2 unsupported (max 1)`

(Same test: file without the key → `E_SCHEMA_VERSION at default.toml:schema_version: missing`; `schema_version = "1"` → `… : must be an integer`.)

**23. Text source with both or neither form**

```toml
schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
context_length = 32768
[prompt]
text = "inline"
file = "prompt.md"
```
Expected: `E_TEXT_SOURCE at stand-in.toml:prompt: exactly one of 'text' or 'file'`

**24. Required key missing after merge** — model profile without `[model].context_length`, kernel defaults have none

```toml
schema_version = 1
[model]
id = "stand-in/default"
endpoint = "litellm-ci"
```
Expected: `E_MISSING_KEY at stand-in.toml:model.context_length`

**25. Unknown placeholder**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${repo}"]
[tools]
allow = ["read"]
```
Expected: `E_UNKNOWN_PLACEHOLDER at default.toml:capabilities.grants[0]: '${repo}'`

(Same test: `"fs.ro:${install:thermal}"` with no `[install].thermal` in bundles.toml.)

**26. Parser slot violated**

```toml
schema_version = 1
[model]
id = "ft/qwen3-32b-2026-08"
endpoint = "vllm-ft"
context_length = 65536
tool_format = "parsed:hermes"
[[middleware]]
name = "tool_call_parser"
priority = 120
```
Expected: `E_PARSER_SLOT at ft-qwen3.toml:middleware[0].priority: tool_call_parser must be 100`

(Same test: `tool_format = "native"` with a declared `tool_call_parser` → `E_PARSER_SLOT … : no parser without parsed tool_format`.)

**27. Catalog reference errors** — `catalog.toml`

```toml
schema_version = 1
[[agents]]
name = "default"
model_profile = "models/stand-in.toml"
agent_profile = "agents/default.toml"
[[agents]]
name = "default"
model_profile = "models/stand-in.toml"
agent_profile = "agents/other.toml"
```
Expected: `E_DUP_NAME at catalog.toml:agents[1].name: 'default'`

(Same test: `[[subagents]] catalog = "nope"` → `E_CATALOG_REF at orchestrator.toml:subagents[0].catalog: 'nope'`; agent file whose `[agent].name` differs from its catalog entry → `E_CATALOG_REF at default.toml:agent.name: 'dflt' != catalog entry 'default'`; `agent_profile` pointing at a missing file → `E_FILE_NOT_FOUND at catalog.toml:agents[0].agent_profile`.)

**28. Derived atom written in grants**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "tool:python"]
[tools]
allow = ["read"]
```
Expected: `E_DERIVED_ATOM_IN_GRANTS at default.toml:capabilities.grants[1]: use [tools].allow`

(Same test for `"spawn:debugger"` → `… use [[subagents]]`.)

**29. Kernel-reserved middleware name**

```toml
schema_version = 1
[agent]
name = "default"
[tools]
allow = ["read"]
[[middleware]]
name = "recorder"
priority = 300
```
Expected: `E_RESERVED_MIDDLEWARE_NAME at default.toml:middleware[0].name: 'recorder'`

**30. HTTP MCP server whose URL host is not in its `net:` atoms**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}", "net:mcp.internal.example"]
[tools]
allow = ["read"]
[[mcp_servers]]
name = "search"
transport = "http"
url = "https://search.internal.example/mcp"
capabilities = ["net:mcp.internal.example"]
```
Expected: `E_MCP_URL_HOST at default.toml:mcp_servers[0].url: 'search.internal.example' not covered`

**31. Referenced file missing**

```toml
schema_version = 1
[agent]
name = "default"
[agent.role_prompt]
file = "missing.md"
[tools]
allow = ["read"]
```
Expected: `E_FILE_NOT_FOUND at default.toml:agent.role_prompt.file: 'missing.md'`

**32. Bundle nests a bundle** — `bundles.toml`

```toml
schema_version = 1
[install]
solver = "/opt/inhouse/solver"
[bundles.solver]
atoms = ["fs.ro:${install:solver}", "proc:sbatch"]
[bundles.all]
atoms = ["solver", "fs.rw:${workdir}"]
```
Expected: `E_MALFORMED_ATOM at bundles.toml:bundles.all.atoms[0]: 'solver' (bundles may not nest)`

**33. Notebook path not writable by grants**

```toml
schema_version = 1
[agent]
name = "default"
[capabilities]
grants = ["fs.rw:${workdir}"]
[tools]
allow = ["read"]
[notebook]
path = "${home}/notes.md"
```
Expected: `E_CAP_EXCEEDS_GRANTS at default.toml:notebook.path: requires 'fs.rw:${home}/notes.md'`

Warnings that P1.8 also tests (as `warns_<slug>`): `W_AGENTS_MD_MISSING`, `W_NET_MASKED` (a `net:` atom with `sandbox.network = false`), `W_SKILL_PATH_MISSING`, `W_PROFILE_DRIFT` (P5.1; emitted as `warning{class: "profile_drift"}`).

---

## 10. Catalog

### 10.1 `profiles/catalog.toml`

```
schema_version = 1
[[agents]]
name          = "<[a-z][a-z0-9_-]*>"   # unique; the spawn name (D6 `Spawn{catalog_name}`)
description   = "<string>"             # optional
model_profile = "<path>"               # relative to this file, or absolute
agent_profile = "<path>"
```

Rules: names unique (`E_DUP_NAME`); both files must exist (`E_FILE_NOT_FOUND`); the agent file's `[agent].name` must equal `name` (`E_CATALOG_REF`); an entry named `default` MUST exist. The same agent profile may appear in several entries with different model profiles (P2.8 registers each task agent with a vLLM, a LiteLLM, and the stand-in model); the same model profile may serve many agents. Promotion (P4.5) appends a new entry and never deletes one.

### 10.2 Shipped content (P1.8)

```toml
schema_version = 1

[[agents]]
name = "default"
description = "General-purpose coding and scripting agent: read, write, edit, bash, run_script, python."
model_profile = "models/stand-in.toml"
agent_profile = "agents/default.toml"
```

with `models/stand-in.toml` exactly as §2.8, `agents/default.toml` and `agents/default.role.md` exactly as §3.11, and `bundles.toml` as §5.2. The resulting tree:

```
profiles/
├── catalog.toml
├── bundles.toml
├── models/
│   └── stand-in.toml
└── agents/
    ├── default.toml
    └── default.role.md
```

P2.8 adds `agents/orchestrator.toml` and `agents/model-debugger.toml` plus `models/vllm-*.toml` and `models/litellm-*.toml`; P4.4 adds `agents/proposer.toml`; P5.1 adds a `models/vllm-ft-*.toml` with `[fine_tune]`. None of those needs a schema change: a debugger is `grants = ["solver", "post"]` (both bundles already carry `fs.rw:${workdir}`) with no `meshing`; a proposer is `grants = ["fs.ro:/archive", "fs.rw:${workdir}"]` with `[agent.eval_set].hidden` pointing outside both; a fine-tune is `tool_format = "parsed:<syntax>"` plus `[fine_tune]`.

---

## 11. Capability atoms

### 11.1 Grammar

ABNF (RFC 5234); `ALPHA`, `DIGIT` as in the core rules. All literals are case-sensitive; atom prefixes are lowercase.

```abnf
capability   = atom / bundle-name

atom         = fs-atom / net-atom / proc-atom / tool-atom / spawn-atom / secret-atom

fs-atom      = ("fs.ro:" / "fs.rw:") path
net-atom     = "net:" ("*" / host-list)
proc-atom    = "proc:" program
tool-atom    = "tool:" tool-name
spawn-atom   = "spawn:" catalog-name
secret-atom  = "secret:" secret-name

path         = (abs-path / placeholder-path)
abs-path     = "/" *pchar
placeholder-path = placeholder *pchar          ; placeholder must be first
placeholder  = "${workdir}" / "${home}" / "${install:" ident "}"
pchar        = %x21-7E                          ; printable ASCII, no space; ".." rejected after parse

host-list    = host *("," host)
host         = hostname [":" port]
hostname     = label *("." label)               ; lowercase; no "*" wildcards in v1
label        = 1*(lower / DIGIT / "-")
port         = 1*5DIGIT

program      = ident / abs-path                 ; bare name resolved on the sandbox PATH, or an absolute path
tool-name    = ident *("." ident)               ; "read", "mcp.docs.search", "ext.sim.mesh_stats"
catalog-name = ident
secret-name  = 1*(ALPHA / DIGIT / "_")          ; conventionally UPPER_SNAKE
bundle-name  = ident                            ; no ":" anywhere; must not equal a reserved prefix
ident        = lower *(lower / DIGIT / "_" / "-")
lower        = %x61-7A
```

Reserved words that may not be bundle names: `fs`, `net`, `proc`, `tool`, `spawn`, `secret`. Classification rule: a string containing `:` is parsed as an atom and must match one of the six prefixes exactly (`fs.ro`, `fs.rw`, `net`, `proc`, `tool`, `spawn`, `secret`); anything else with a `:` is `E_MALFORMED_ATOM`. A string without `:` is a bundle name.

Regex form of the same, for a quick pre-check (not a substitute for the parser):

```
^(fs\.r[ow]:(/|\$\{(workdir|home|install:[a-z][a-z0-9_-]*)\})[^\s]*
 |net:(\*|[a-z0-9.-]+(:\d{1,5})?(,[a-z0-9.-]+(:\d{1,5})?)*)
 |proc:([a-z][a-z0-9_-]*|/[^\s]+)
 |tool:[a-z][a-z0-9_-]*(\.[a-z][a-z0-9_-]*)*
 |spawn:[a-z][a-z0-9_-]*
 |secret:[A-Za-z0-9_]+
 |[a-z][a-z0-9_-]*)$
```

### 11.2 Mapping to `kernel::Capability`

| String form | Rust value (kernel-interface.md) |
|---|---|
| `fs.ro:<p>` / `fs.rw:<p>` | `Capability::Fs { path: PathBuf(<p>), mode: FsMode::Ro }` / `FsMode::Rw` |
| `net:*` | `Capability::Net { allow: NetAllow::Any }` |
| `net:h1,h2` | `Capability::Net { allow: NetAllow::Hosts({h1, h2}) }` (a `BTreeSet<String>`) |
| `proc:<prog>` | `Capability::Proc { program: <prog> }` |
| `tool:<name>` | `Capability::Tool { name }` |
| `spawn:<name>` | `Capability::Spawn { catalog_name }` |
| `secret:<NAME>` | `Capability::Secret { name }` |

`Display` for `Capability` produces the canonical string form (after normalization); `FromStr` parses it. Round-trip `parse(display(c)) == c` is a P1.1 property test. The string form is what appears in every event payload (`tool.capabilities`, `spawn.ceiling`, `E_CAP_EXCEEDS_GRANTS` messages), never the Rust debug form.

### 11.3 Normalization

Applied after placeholder expansion, before parsing into `Capability`, and by `Display`:

| Kind | Rule |
|---|---|
| paths | must start with `/`; collapse repeated `/`; remove `.` segments; **reject** `..` (`E_PATH_DOTDOT`) — it is not resolved, because resolution depends on symlinks the validator does not follow; strip trailing `/` except for the root; no symlink resolution (bwrap binds the literal path; a symlink inside a bind pointing outside it dangles, so the prefix check stays sound) |
| hosts | lowercase; strip one trailing `.`; sort and dedupe the list; `*` may not be combined with hosts |
| programs | bare names unchanged; absolute paths normalized as paths |
| tool, catalog, secret names | unchanged (grammar already fixes case for tool and catalog names) |
| grant lists | dedupe, then sort by canonical string form |

### 11.4 `narrower_than`

`a.narrower_than(b)` is true when everything `a` permits, `b` also permits. It is reflexive and transitive within a variant; **different variants are never comparable** (an `fs` atom is never narrower than a `proc` atom, however generous). Definition per variant:

| Variant | `a ≤ b` iff | Examples (true) | Examples (false) |
|---|---|---|---|
| `Fs` | `a.path == b.path` or `a.path` is under `b.path` (component-wise prefix after normalization) **and** `a.mode ≤ b.mode` where `Ro ≤ Rw` | `fs.ro:/w/src ≤ fs.rw:/w`; `fs.ro:/w ≤ fs.rw:/w`; `fs.rw:/w/a/b ≤ fs.rw:/w` | `fs.rw:/w ≤ fs.ro:/w` (mode); `fs.ro:/work ≤ fs.ro:/w` (string prefix is not path prefix); `fs.ro:/ ≤ fs.rw:/w` |
| `Net` | `b = Any`, or both `Hosts` and `a.hosts ⊆ b.hosts` where a host with a port is under the same host without one | `net:a.example ≤ net:*`; `net:a.example ≤ net:a.example,b.example`; `net:a.example:443 ≤ net:a.example` | `net:* ≤ net:a.example`; `net:a.example ≤ net:a.example:443`; `net:sub.a.example ≤ net:a.example` (no wildcard semantics) |
| `Proc` | `a.program == b.program` | `proc:sbatch ≤ proc:sbatch` | `proc:/usr/bin/sbatch ≤ proc:sbatch` (different spelling is a different program in v1) |
| `Tool` | exact name | `tool:read ≤ tool:read` | `tool:mcp.docs.search ≤ tool:mcp.docs.*` (no globs) |
| `Spawn` | exact catalog name | | |
| `Secret` | exact name | | |

`covered(a, grants)` — the check used everywhere in §7.2 step 8 — is `grants.iter().any(|g| a.narrower_than(g))`. A single required atom is never satisfied by the *union* of two grants (e.g. `fs.rw:/w` is not covered by `fs.ro:/w` plus `fs.rw:/w/sub`); requirements must be declared at the granularity they need.

Property tests (P1.1): reflexive; transitive; antisymmetric up to normalization; `Ro ≤ Rw` monotone with path; `Hosts ⊆` monotone; incomparable variants both ways.

### 11.5 Tool-name convention

- First-party tools: single `snake_case` identifiers — `read`, `write`, `edit`, `bash`, `run_script`, `python` (P1.7); `find_tools` (P2.2), `spawn`, `request_capability` (P2.4), `read_artifact` (P2.5), `ask_user` (D17, P1.6).
- MCP tools: `mcp.<server-name>.<tool>` where `<server-name>` is the `[[mcp_servers]].name` and `<tool>` is the server's name for it, lowercased with non-identifier characters mapped to `_`.
- Out-of-process extension tools (P2.1): `ext.<manifest-name>.<tool>`.
- The prefixes `mcp.` and `ext.` are reserved; a first-party tool may not start with them.
- A skill is not a tool and has no `tool:` atom; it references tools by these names.

---

## 12. Schema versioning

- Every profile-family file — model profile, agent profile, project override, `bundles.toml`, `catalog.toml`, and (from P2.3) skill frontmatter — carries a top-level `schema_version = 1` integer. It is required (`E_SCHEMA_VERSION … missing`).
- All six file kinds share **one** version number, bumped together, so a reader never has to reason about cross-file compatibility. This spec is version `1`.
- A file with a version greater than the loader supports is rejected (`E_SCHEMA_VERSION … unsupported`); a lower version is accepted only if a migration is registered for it, else rejected the same way. P1.8 ships no migrations (there is nothing older); the migration hook and its test are the "Schema versioning" cross-cutting track in the implementation plan.
- The `ResolvedProfile` struct (§7.6) carries `schema_version` too, so a resolved hash from a future version can never collide with one from this version.
- Adding an *optional* key with a default is a compatible change and does **not** bump the version (an old file still resolves identically). Renaming, removing, retyping a key, changing a default, or changing the resolved-struct shape bumps it.

---

## 13. Open questions for the human reviewer

1. **Hash before or after placeholder expansion (§7.2 step 6, §7.6).** This spec hashes the *symbolic* resolved profile so the same profile hashes identically in every checkout and the machine binding lives in `sandbox_policy_hash`. The alternative — hashing expanded paths — makes provenance more literal but makes every fine-tune drift check and every cross-machine eval comparison fail spuriously. Confirm.
2. **Derived `tool:` / `spawn:` atoms (§3.2).** `[tools].allow` and `[[subagents]]` are the single source of `Tool` / `Spawn` atoms; writing them in `grants` is an error. Alternative: allow both and require consistency. Confirm the single-source rule.
3. **Strict D6 for sub-agent tools (§3.6).** Taken literally, a parent must list every tool its children use in its own `[tools].allow`, which puts those tools in the parent's prompt. A `[tools].delegable = [...]` key (tools the parent may grant but not call) would fix the context cost at the price of one more concept. Decide before P2.4; P1.8 is unaffected.
4. **Endpoint URL resolution (§2.1).** `GRIST_ENDPOINT_<NAME>_URL` from the launcher's environment is the P1 answer, chosen so URLs never enter a hashed file. If the human prefers a `profiles/endpoints.toml`, it should be explicitly *not* a `ProfileLoad` kind and not hashed into any profile.
5. **`proc:sbatch` and the network namespace (§5.2).** Slurm client commands talk to `slurmctld` over TCP; under `network = false` they will fail on a real cluster. Either the P0.1 spike shows bwrap with `--share-net` scoped by the atom, or the bundles gain `net:<slurmctld-host>` and `sandbox.network = true`. P3.4 decides; the bundles file shape does not change.
6. **Stand-in model id (§2.8).** `stand-in/default` as a LiteLLM route so the CI model can change without a profile change. P0.5 must configure that route name. Alternative: the real model name, at the cost of a hash change every time CI's model changes.
7. **Project overrides of `tool_descriptions` and `model.temperature` (§4.2).** Allowed here as "values". If the reviewer wants the model axis fully sealed from the workdir, move those rows to forbidden.
8. **`sandbox.network = false` with `net:` grants (§3.9).** Specified as a warning plus masking, not an error, so a project override can switch the network off without also rewriting `grants`. Confirm the warning is enough.
8b. **Provider retry keys.** This version defines no `provider.retry.*` keys; the kernel's `RetryPolicy` default (5 attempts, 500 ms base, ×2, 30 s cap, full jitter, 300 s per attempt; kernel-interface.md §9) applies to every session. If per-model tuning is wanted, add a `[model.retry]` table in 0.2 and map it in kernel-interface.md §7.8.
9. **`spill_cap_bytes` default 16384 (§1.4).** Roughly 4k tokens against a 40k budget. Not derived from measurement; P2.9 should revisit.
10. **Middleware `config` replaced, not deep-merged, on override (§6.3).** Chosen so a middleware never receives a config union it did not anticipate. This is a narrow reading of D7's "tables deep-merge"; confirm it does not need an ADR.
11. **`AGENTS.md` read once per session (§8.3).** Keeps model calls reproducible from the checkpoint; the cost is that an agent editing `AGENTS.md` sees the effect only after resume. Alternative: re-read per turn and log its hash per call.
12. **Host ports and wildcards in `net:` (§11.1).** v1 has `host[:port]`, no wildcards, no CIDR. Sufficient for a LiteLLM proxy and an MCP server; probably insufficient for `pip install`. Extend when a real need appears, as a compatible grammar addition.
13. **`[install]` inside `bundles.toml` with no environment override (§5.1).** Means CI must install the mock solver at `/opt/inhouse/solver` inside its container. If that is awkward for P0.5, the alternative is a launcher-supplied install map, which would also have to be hashed into `bundles_hash`.
14. **`E_HIDDEN_EVAL_REACHABLE` scope (§3.1).** Checks the workdir and this profile's own `fs` grants. It cannot check other profiles that might be spawned; P4.2's runtime test still has to exist.
