# Event and hash schema specification

- **Status:** approved at v0.1 (2026-09-06); implementation clarifications are marked **[clarified in P1.x]**
- **Version:** 0.1
- **Date:** 2026-09-06
- **Milestone:** P1.0 (D20). Implemented by P1.1 (`Event` enum, hashing module), P1.3 (log writer, redactor, checkpoints, recovery), P1.4 (record/replay, `diff-logs`).
- **Companion specs:** [`kernel-interface.md`](./kernel-interface.md) (Rust types referenced here: `State`, `Message`, `ContentBlock`, `ToolOutput`, `TaskUpdate`, …), [`profile-schema.md`](./profile-schema.md) (how profile hashes are computed).

The event log is simultaneously the session file, the checkpoint stream, the replay cassette, and the provenance feed (dev plan §4.2). Every rule below serves at least one of those four readers. Rust type names are those of `kernel-interface.md` §3; where this document shows a payload struct, that struct is the one `EventBody` carries (`kernel-interface.md` §3.14). Paragraphs marked **[decided here]** are choices D1–D20 do not literally settle; they are collected in §8.

---

## 1. Envelope, framing, versioning, ordering

### 1.1 Envelope (D3)

Every line is one JSON object with exactly these five members, in this order:

| Field | Type | Meaning |
|---|---|---|
| `seq` | `u64` | Position in this session's log. Starts at `0`, strictly increasing by 1 per physical line. |
| `ts` | string | Wall-clock time the writer assigned the event, RFC 3339 UTC with millisecond precision: `2026-09-06T12:34:56.789Z`. Informational; never hashed. |
| `session_id` | string | The session this log belongs to. Identical on every line of a file. |
| `kind` | string | Event kind, `snake_case`, one of §2. |
| `payload` | object | Kind-specific. **Hashes cover `payload` only** (D3); envelope fields are never hashed. |

```json
{"seq":7,"ts":"2026-09-06T12:34:56.789Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"user_message","payload":{"turn":0,"applied":{"turn":0,"at":"idle"},"content":[{"type":"text","text":"Mesh the bracket at 2 mm."}]}}
```

The Rust side is `Event { seq, ts, session_id, #[serde(flatten)] body: EventBody }` with `EventBody` tagged `kind`/`payload`. The serialized member order above is what `serde` produces from that struct; readers MUST NOT depend on member order.

### 1.2 JSONL framing

- One event per line; lines end with `\n` (0x0A); the file is UTF-8 without BOM.
- A line contains no raw control characters (JSON escaping guarantees this), so `\n` is an unambiguous record separator.
- **Never rewritten.** The writer only appends. There is no compaction, truncation, or in-place edit of a log file, ever. Recovery (§6.2) voids a seq range logically by appending a `recovered` event; the voided lines stay in the file.
- One file per session: `<session_id>.jsonl`. The path is the launcher's; the kernel only needs it to exist and be writable.
- The writer MUST `fsync` after every `checkpoint` line before reporting the append as complete (durability of the last-good state); it SHOULD flush other lines promptly.
- A reader MUST tolerate a truncated final line (a crash mid-write): it is treated as absent, and `seq` continues from the last complete line. A reader MUST fail on a malformed line that is not the last line (`LogError::Malformed`).

**[clarified in P1.3]** `open` on an empty file, or one whose line 0 is not `log_opened`, is `LogError::Malformed{line: 1}`; `create` on an existing path is `LogError::Io` (never clobbers). A torn final line is dropped by truncating the file to the last complete newline on `open`, the only in-place operation ever performed. `NewerSchema` is also raised when a `resumed`/`recovered` line stamps a newer `event_schema_version`. The writer also fsyncs `suspended`, `session_ended`, and `session_failed`. A `checkpoint` whose `state` no longer deserializes is kept raw only when its `schema_version` is older than the kernel's, so `restore` can migrate it; any other undeserializable payload of a known kind is `Malformed`. On-disk `payload` member order is alphabetical; envelope members keep the §1.1 order.

### 1.3 Schema version

`EVENT_SCHEMA_VERSION: u32 = 1` (`kernel-interface.md` §3.1). It is recorded in the first event of every log, `log_opened` (§2.1), together with `STATE_SCHEMA_VERSION`. Rules:

- The event schema version is bumped when the envelope changes, when a payload field changes meaning or type, or when a field is removed. **Adding** an optional field to a payload, or adding a new `kind`, does not bump it: readers MUST ignore unknown payload fields and MUST NOT fail on unknown kinds (`EventBody::Unknown`).
- A kernel MUST refuse to open a log whose `event_schema_version` is greater than its own (`LogError::NewerSchema`). Reading older versions is a projector/replay concern; the kernel only needs to read its own version's `checkpoint` payloads, and those carry their own `State.schema_version` (§6.3).
- The version is per file, fixed at `log_opened`. A kernel with a newer event schema that resumes an older log writes a new log file? **No [decided here]:** it appends to the same file, and the `resumed`/`recovered` event carries `event_schema_version` so a reader knows the version changes at that seq. Justification: one file per session is a stronger invariant than one version per file, and version bumps are rare.

### 1.4 Ordering

- `seq` is strictly increasing from `0` per session, with no gaps in the physical file. A reader that sees `seq != previous + 1` MUST fail (`LogError::SeqOrder`).
- Within a turn the kernel writes events in the order of `kernel-interface.md` §6. The minimal shape of one turn with one tool call is: `model_request`, `model_response`, `tool_call`, `tool_result`, (`task_started`), `checkpoint`.
- A session log begins `log_opened`, `session_created`, `profile_load`*, `middleware_chain_resolved`, (`warning`)*, and its first `checkpoint` is written when the first user message is applied.
- Events written by extensions through `HookContext::emit` appear at the point the hook ran (inside the turn); they never reorder kernel events.

---

## 2. Event kinds

Conventions in the field tables: **R** = required (always present), **O** = optional (`null` or absent; serde `Option`), **V** = volatile for `diff-logs` (stripped before comparison, §5.3). Hash fields are `b3:<64 hex>` strings. `turn` is `State.turn` at the time of writing. All examples share `session_id` `s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4`; hashes are illustrative.

Kind list (28): `log_opened`, `session_created`, `profile_load`, `middleware_chain_resolved`, `user_message`, `model_request`, `model_response`, `provider_retry`, `tool_call`, `tool_result`, `task_started`, `task_update`, `checkpoint`, `suspended`, `resumed`, `cancelled`, `turn_failed`, `session_failed`, `session_ended`, `recovered`, `compaction`, `spawn`, `child_completed`, `context_usage`, `harness_edit`, `ask_user`, `user_answer`, `warning`. Kinds writable by extensions (`ExtensionEvent`): `profile_load`, `context_usage`, `spawn`, `child_completed`, `harness_edit`, `warning`. All others are kernel-only.

### 2.1 `log_opened`

First event of every log file, `seq: 0`. Written by `FileEventLog::create`.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `event_schema_version` | u32 | R | `EVENT_SCHEMA_VERSION` |
| `state_schema_version` | u32 | R | `STATE_SCHEMA_VERSION` |
| `kernel_version` | string | R, V | `KERNEL_VERSION` |
| `mode` | `"live" \| "replay"` | R, V | replay logs are written by a kernel driven by `ReplayDriver` |

```rust
pub struct LogOpenedPayload { pub event_schema_version: u32, pub state_schema_version: u32, pub kernel_version: String, pub mode: LogMode }
pub enum LogMode { Live, Replay }
```
```json
{"seq":0,"ts":"2026-09-06T12:00:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"log_opened","payload":{"event_schema_version":1,"state_schema_version":1,"kernel_version":"0.0.0","mode":"live"}}
```

### 2.2 `session_created`

Once per session, `seq: 1`. This is the PROV *Agent* identity record (§7).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `session_id` | string | R, V | duplicates the envelope so the payload is self-describing for the projector |
| `created_at` | string | R, V | RFC 3339 |
| `kernel_version` | string | R, V | |
| `profiles` | `ActiveProfiles` | R | `{model_profile_hash, agent_profile_hash, resolved_profile_hash}` |
| `model_id` | string | R | from the model profile |
| `tools` | string[] | R | registered tool names, sorted |
| `grants` | string[] | R | capability atoms in canonical string form, sorted |
| `sandbox_backend` | string | R, V | `SandboxBackend::name()` (D14) |
| `sandbox_policy_hash` | hash | R, V | §3.7 |
| `artifact_store` | string | R, V | `ArtifactStore::name()` |
| `memory` | string | R, V | `Memory::name()` |
| `provider` | string | R, V | `Provider::name()` |
| `spill` | `SpillConfig` | R | after clamping |
| `retry` | `RetryPolicy` | R | |
| `notebook_path` | string | O | |
| `sandbox_limits` | `SandboxLimits` | R | `{timeout_s, scratch_tmpfs_mb, env_allow, network}` |
| `overrides` | any | O | layer-4 runtime overrides from the start-session message (`profile-schema.md` §4.4) |
| `parent` | object | O | `{session_id, task_id}` when spawned by P2.4; `null` for root sessions |

```rust
pub struct SessionCreatedPayload {
    pub session_id: SessionId, pub created_at: String, pub kernel_version: String,
    pub profiles: ActiveProfiles, pub model_id: String, pub tools: Vec<String>, pub grants: Vec<Capability>,
    pub sandbox_backend: String, pub sandbox_policy_hash: Hash, pub artifact_store: String, pub memory: String,
    pub provider: String, pub spill: SpillConfig, pub retry: RetryPolicy, pub notebook_path: Option<PathBuf>,
    pub sandbox_limits: SandboxLimits, pub overrides: Option<Value>, pub parent: Option<ParentRef>,
}
pub struct ParentRef { pub session_id: SessionId, pub task_id: TaskId }
```
```json
{"seq":1,"ts":"2026-09-06T12:00:00.003Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"session_created","payload":{"session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","created_at":"2026-09-06T12:00:00.001Z","kernel_version":"0.0.0","profiles":{"model_profile_hash":"b3:1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","agent_profile_hash":"b3:2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b","resolved_profile_hash":"b3:3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c","project_profile_hash":null,"bundles_hash":"b3:bdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbd"},"model_id":"stand-in/qwen2.5-7b-instruct","tools":["bash","edit","python","read","run_script","write"],"grants":["fs.ro:/opt/inhouse","fs.rw:/work/bracket","proc:sbatch","tool:bash","tool:edit","tool:python","tool:read","tool:run_script","tool:write"],"sandbox_backend":"bwrap","sandbox_policy_hash":"b3:4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d","artifact_store":"noop","memory":"noop","provider":"openai_compat","spill":{"cap_bytes":16384,"head_bytes":2048,"tail_bytes":2048},"retry":{"max_attempts":5,"base_delay_ms":500,"max_delay_ms":30000,"multiplier":2.0,"jitter":true,"request_timeout_secs":300},"notebook_path":"/work/bracket/.grist/notebook.md","sandbox_limits":{"timeout_s":600,"scratch_tmpfs_mb":256,"env_allow":["LANG","PATH","TERM"],"network":false},"overrides":null,"parent":null}}
```

### 2.3 `profile_load`

One per loaded profile-like artifact: written by the kernel from `SessionInit.profile_loads` right after `session_created`, and by extensions (`emit`) when a skill or sub-agent definition is loaded later (P2.3, P2.4).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `kind` | `"model" \| "agent" \| "project" \| "bundles" \| "catalog" \| "skill" \| "subagent"` | R | the five `profile-schema.md` kinds plus `catalog` and `subagent` (P2.4 definitions) |
| `name` | string | R | profile/skill name |
| `path` | string | O, V | where it was read from (absolute path differs across machines) |
| `hash` | hash | R | `b3` of the source file bytes (`profile-schema.md` §7.2 step 1); opaque here |
| `rejected` | bool | R | `true` when the loader refused it (e.g. a skill whose capabilities exceed grants) but logs the attempt |
| `turn` | u64 | R | `0` for start-up loads |

```rust
pub struct ProfileLoadPayload { pub kind: ProfileKind, pub name: String, pub path: Option<PathBuf>, pub hash: Hash, pub rejected: bool, pub turn: u64 }
pub enum ProfileKind { Model, Agent, Project, Bundles, Catalog, Skill, Subagent }
```
```json
{"seq":2,"ts":"2026-09-06T12:00:00.004Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"profile_load","payload":{"kind":"model","name":"stand-in/qwen2.5-7b-instruct","path":"/work/bracket/profiles/models/stand-in.toml","hash":"b3:1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","rejected":false,"turn":0}}
```

### 2.4 `middleware_chain_resolved`

At `create` and at every `open` (D7, dev plan §14 "resolved chain logged into every run").

| Field | Type | R/O | Notes |
|---|---|---|---|
| `chain` | `[{index: u32, name: string, priority: i32, source: "kernel" \| "model" \| "agent" \| "project", config_hash: hash \| null}]` | R | in execution order (same order for every hook, `kernel-interface.md` §7.6) |
| `chain_hash` | hash | R | hash of `chain` |

```rust
pub struct MiddlewareChainResolvedPayload { pub chain: Vec<ChainEntry>, pub chain_hash: Hash }
pub struct ChainEntry { pub index: u32, pub name: String, pub priority: i32, pub source: MiddlewareSource, pub config_hash: Option<Hash> }
```
```json
{"seq":6,"ts":"2026-09-06T12:00:00.010Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"middleware_chain_resolved","payload":{"chain":[{"index":0,"name":"tool_call_parser","priority":100,"source":"model","config_hash":"b3:cfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcfcf"},{"index":1,"name":"compaction","priority":300,"source":"agent","config_hash":"b3:c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0"},{"index":2,"name":"recorder","priority":990,"source":"kernel","config_hash":null}],"chain_hash":"b3:5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e"}}
```

### 2.5 `user_message`

Written when a user message is **applied** to `State.messages`, not when it was received. **[decided here]** Justification: the log position must be deterministic relative to kernel events for `diff-logs` and for `Cassette::from_log`; receipt time is timing-dependent. `applied` tells the replay driver when to re-deliver it (`kernel-interface.md` §3.16).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | `State.turn` at application |
| `applied` | `{turn: u64, at: "idle" \| "suspended" \| "failed" \| "end_of_turn" \| "created"}` | R | where in the session it was applied |
| `content` | `ContentBlock[]` | R | post-redaction |

```rust
pub struct UserMessagePayload { pub turn: u64, pub applied: AppliedAt, pub content: Vec<ContentBlock> }
```
```json
{"seq":7,"ts":"2026-09-06T12:00:05.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"user_message","payload":{"turn":0,"applied":{"turn":0,"at":"created"},"content":[{"type":"text","text":"Mesh the bracket at 2 mm and run the static case."}]}}
```

### 2.6 `model_request` (D13)

Once per turn, before the first provider attempt. Carries hashes and a summary, not the full request: the request is reconstructable from the preceding `checkpoint` plus the middleware chain, and logging it would make the log O(turns²). **[decided here]**

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `request_hash` | hash | R | §3.1 |
| `checkpoint_hash` | hash | R | `state_hash` of the checkpoint this request was computed from; replay key |
| `model_id` | string | R | as sent |
| `profiles` | `ActiveProfiles` | R | active profile hashes (D13) |
| `system_prompt_hash` | hash | R | hash of `req.system` (block list) |
| `prompt_blocks` | `[{kind, name, hash}]` | R | one per `PromptBlock`, in order; `kind` ∈ `model`, `role`, `agents_md`, `skills`, `notebook` (D7); lets P3.1 say which role prompt and skills a call used without re-parsing |
| `message_count` | u32 | R | `req.messages.len()` |
| `tool_names` | string[] | R | `req.tools` names in order (lazy exposure makes this vary per turn) |
| `params_hash` | hash | R | hash of `req.params` |

```rust
pub struct ModelRequestPayload { pub turn: u64, pub request_hash: Hash, pub checkpoint_hash: Hash, pub model_id: String, pub profiles: ActiveProfiles, pub system_prompt_hash: Hash, pub prompt_blocks: Vec<PromptBlockRef>, pub message_count: u32, pub tool_names: Vec<String>, pub params_hash: Hash }
pub struct PromptBlockRef { pub kind: PromptBlockKind, pub name: String, pub hash: Hash }
```
```json
{"seq":9,"ts":"2026-09-06T12:00:05.020Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"model_request","payload":{"turn":1,"request_hash":"b3:6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f","checkpoint_hash":"b3:7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a","model_id":"stand-in/qwen2.5-7b-instruct","profiles":{"model_profile_hash":"b3:1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","agent_profile_hash":"b3:2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b","resolved_profile_hash":"b3:3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c","project_profile_hash":null,"bundles_hash":"b3:bdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbd"},"system_prompt_hash":"b3:8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b","prompt_blocks":[{"kind":"model","name":"stand-in","hash":"b3:a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1"},{"kind":"role","name":"default","hash":"b3:a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2"},{"kind":"agents_md","name":"/work/bracket/AGENTS.md","hash":"b3:a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3"}],"message_count":1,"tool_names":["read","write","edit","bash","run_script","python"],"params_hash":"b3:9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c"}}
```

### 2.7 `model_response` (D13)

Once per successful provider call (after retries). Carries the **full content blocks** so the log is a complete cassette (§5).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `request_hash` | hash | R | correlates with `model_request` |
| `response_hash` | hash | R | §3.2 |
| `raw_response_hash` | hash | R | `Hash::of_bytes` of the raw provider body; provenance only |
| `model_id` | string | R | what the endpoint reported it served |
| `stop_reason` | `StopReason` | R | |
| `usage` | `Usage` | R | `{input_tokens, output_tokens, cache_read_tokens, reasoning_tokens}` (D13) |
| `content` | `ContentBlock[]` | R | post-redaction, post-`after_model` (parsed tool calls appear as `tool_use` blocks) |
| `attempts` | u32 | R, V | how many provider attempts this took (1 = no retry) |

```rust
pub struct ModelResponsePayload { pub turn: u64, pub request_hash: Hash, pub response_hash: Hash, pub raw_response_hash: Hash, pub model_id: String, pub stop_reason: StopReason, pub usage: Usage, pub content: Vec<ContentBlock>, pub attempts: u32 }
```
```json
{"seq":10,"ts":"2026-09-06T12:00:08.410Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"model_response","payload":{"turn":1,"request_hash":"b3:6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f","response_hash":"b3:0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d","raw_response_hash":"b3:1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e","model_id":"qwen2.5-7b-instruct","stop_reason":"tool_use","usage":{"input_tokens":1834,"output_tokens":92,"cache_read_tokens":null,"reasoning_tokens":null},"content":[{"type":"text","text":"I'll start the meshing job."},{"type":"tool_use","id":"call_8f2a","name":"run_script","input":{"path":"/work/bracket/mesh.sh","args":["--size","2mm"]}}],"attempts":1}}
```

Timing of `content` **[decided here]:** the event is written **after** the `after_model` chain (step D2 in `kernel-interface.md` §6), so `content` is exactly what the kernel appended to `State.messages`, and for a `parsed`-format model the parsed `tool_use` blocks are present. The pre-parse provider output is covered only by `raw_response_hash`. This is what makes the log a complete cassette for parsed-format models (§5.1).

### 2.8 `provider_retry`

One per retry attempt (D15). Entire kind is volatile for `diff-logs` (replay never retries).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `attempt` | u32 | R | the attempt that failed (1-based) |
| `error_class` | string | R | `ProviderError::class()` |
| `message` | string | R | redacted |
| `delay_ms` | u64 | R | back-off before the next attempt |

```rust
pub struct ProviderRetryPayload { pub turn: u64, pub attempt: u32, pub error_class: String, pub message: String, pub delay_ms: u64 }
```
```json
{"seq":31,"ts":"2026-09-06T12:03:10.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"provider_retry","payload":{"turn":4,"attempt":1,"error_class":"provider_rate_limited","message":"429 Too Many Requests","delay_ms":412}}
```

### 2.9 `tool_call` (D13)

One per tool call the model made, registered or not. Written after `before_tool` (so an edited `input` is what is logged).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `tool_use_id` | string | R | from the `tool_use` block |
| `name` | string | R | |
| `args_hash` | hash | R | §3.3 |
| `request_hash` | hash | R | §3.3, replay key for the tool result |
| `checkpoint_hash` | hash | R | replay key, first half |
| `input` | any | R | the (possibly middleware-edited) arguments, redacted |
| `registered` | bool | R | `false` → no invoke happened (`kernel-interface.md` §7.7) |
| `kind` | `"stateless" \| "session"` | O | `null` when unregistered |
| `capabilities` | string[] | R | the tool's atoms, canonical strings; `[]` when unregistered |
| `policy_hash` | hash | O, V | hash of the derived `SandboxPolicy`; `null` when unregistered |

```rust
pub struct ToolCallPayload { pub turn: u64, pub tool_use_id: String, pub name: String, pub args_hash: Hash, pub request_hash: Hash, pub checkpoint_hash: Hash, pub input: Value, pub registered: bool, pub kind: Option<ToolKind>, pub capabilities: Vec<Capability>, pub policy_hash: Option<Hash> }
```
```json
{"seq":11,"ts":"2026-09-06T12:00:08.415Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"tool_call","payload":{"turn":1,"tool_use_id":"call_8f2a","name":"run_script","args_hash":"b3:2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f","request_hash":"b3:3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a","checkpoint_hash":"b3:7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a","input":{"path":"/work/bracket/mesh.sh","args":["--size","2mm"]},"registered":true,"kind":"stateless","capabilities":["fs.rw:/work/bracket","proc:bash"],"policy_hash":"b3:4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b4b"}}
```

### 2.10 `tool_result` (D13)

One per `tool_call`, always, including unregistered, cancelled, and timed-out calls. Carries the **post-spill, post-redaction content** the model sees, so the log is the tool cassette. **[decided here]** (D13 asks for the hash and the handles; adding the bounded content costs at most `spill_cap_bytes` per call and removes the need for a second cassette store.)

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `tool_use_id` | string | R | |
| `name` | string | R | |
| `result_hash` | hash | R | §3.4 |
| `is_error` | bool | R | |
| `content` | `ToolResultContent` | R | `{"json": …}` or `{"blocks": […]}`; post-spill |
| `artifact_handles` | hash[] | R | every handle in the result: spill handle, `Image` blocks, handles a tool reports explicitly |
| `spilled` | bool | R | |
| `spill` | `{handle, size, mime}` | O | present iff `spilled` |
| `duration_ms` | u64 | R, V | wall time of `invoke` (0 for `middleware`/`unregistered`) |
| `origin` | `"invoke" \| "middleware" \| "unregistered" \| "cancelled" \| "timeout"` | R | `ToolOutputOrigin`; copied verbatim on replay |
| `task` | `{task_id, status}` | O | when the result was a task handle |

```rust
pub struct ToolResultPayload { pub turn: u64, pub tool_use_id: String, pub name: String, pub result_hash: Hash, pub is_error: bool, pub content: ToolResultContent, pub artifact_handles: Vec<ArtifactHandle>, pub spilled: bool, pub spill: Option<SpillRef>, pub duration_ms: u64, pub origin: ToolOutputOrigin, pub task: Option<TaskRef> }
pub struct SpillRef { pub handle: ArtifactHandle, pub size: u64, pub mime: String }
pub struct TaskRef { pub task_id: TaskId, pub status: TaskStatus }
```
```json
{"seq":12,"ts":"2026-09-06T12:00:08.530Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"tool_result","payload":{"turn":1,"tool_use_id":"call_8f2a","name":"run_script","result_hash":"b3:5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c","is_error":false,"content":{"json":{"task_id":"t1-call_8f2a","status":"running","eta_secs":600,"description":"mesh.sh --size 2mm (pid 41022)"}},"artifact_handles":[],"spilled":false,"spill":null,"duration_ms":112,"origin":"invoke","task":{"task_id":"t1-call_8f2a","status":"running"}}}
```

A spilled example (`bash` output over the cap):

```json
{"seq":19,"ts":"2026-09-06T12:01:02.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"tool_result","payload":{"turn":2,"tool_use_id":"call_91bb","name":"bash","result_hash":"b3:6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d6d","is_error":false,"content":{"json":{"handle":"b3:7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e","head":"{\"exit_code\":0,\"stdout\":\"Reading mesh.log\\nstep 1 ...","tail":"... converged in 143 iterations\\n\",\"stderr\":\"\"}","size":48211,"mime":"application/json"}},"artifact_handles":["b3:7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e"],"spilled":true,"spill":{"handle":"b3:7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e7e","size":48211,"mime":"application/json"},"duration_ms":2380,"origin":"invoke","task":null}}
```

### 2.11 `task_started`

Right after the `tool_result` whose `task` is set (D1).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `task_id` | string | R | `t<turn>-<tool_use_id>` |
| `tool_use_id` | string | R | |
| `tool_name` | string | R | |
| `status` | `"pending" \| "running"` | R | |
| `eta_secs` | u64 | O | |
| `description` | string | O | |
| `check_hint` | any | O | for the polling sidecar (P3.4); never in the model context, but in the log |
| `in_process_waker` | bool | R | `true` for P1 `run_script` |

```rust
pub struct TaskStartedPayload { pub turn: u64, pub task_id: TaskId, pub tool_use_id: String, pub tool_name: String, pub status: TaskStatus, pub eta_secs: Option<u64>, pub description: Option<String>, pub check_hint: Option<Value>, pub in_process_waker: bool }
```
```json
{"seq":13,"ts":"2026-09-06T12:00:08.531Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"task_started","payload":{"turn":1,"task_id":"t1-call_8f2a","tool_use_id":"call_8f2a","tool_name":"run_script","status":"running","eta_secs":600,"description":"mesh.sh --size 2mm (pid 41022)","check_hint":{"pid":41022,"log":"/work/bracket/mesh.log"},"in_process_waker":true}}
```

### 2.12 `task_update`

One per `TaskUpdate` **applied** (or ignored) by the kernel (D1). Like `user_message`, written at application time, with `applied` for the replay schedule.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `task_id` | string | R | |
| `from_status` | `TaskStatus` | O | `null` when the task is unknown |
| `status` | `TaskStatus` | R | the update's status |
| `outcome` | `TaskOutcome` | O | `{content, is_error, artifact_handles}`; present for terminal updates |
| `eta_secs` | u64 | O | |
| `check_hint` | any | O | |
| `waker` | `WakerSource` | R, V | `{kind, trust_tier, detail}`; `trust_tier` is `null` until P3.4 |
| `applied` | `{turn, at}` | R | as in §2.5, plus `at: "ignored"` for terminal/unknown/invalid |
| `ignored_reason` | string | O | `"terminal"`, `"unknown_task"`, `"missing_outcome"` |

```rust
pub struct TaskUpdatePayload { pub turn: u64, pub task_id: TaskId, pub from_status: Option<TaskStatus>, pub status: TaskStatus, pub outcome: Option<TaskOutcome>, pub eta_secs: Option<u64>, pub check_hint: Option<Value>, pub waker: WakerSource, pub applied: AppliedAt, pub ignored_reason: Option<String> }
```
```json
{"seq":24,"ts":"2026-09-06T12:09:41.200Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"task_update","payload":{"turn":3,"task_id":"t1-call_8f2a","from_status":"running","status":"succeeded","outcome":{"content":{"json":{"exit_code":0,"log":"/work/bracket/mesh.log","elements":184220}},"is_error":false,"artifact_handles":[]},"eta_secs":null,"check_hint":null,"waker":{"kind":"in_process_exit","trust_tier":null,"detail":{"pid":41022}},"applied":{"turn":3,"at":"suspended"},"ignored_reason":null}}
```

### 2.13 `checkpoint`

The durable state record. Written at the end of every turn and on every out-of-turn state change (`kernel-interface.md` §7.8).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `reason` | `"user_input" \| "turn_end" \| "task_update" \| "resume" \| "recovery" \| "compaction" \| "cancel" \| "failure" \| "end" \| "suspend"` | R | |
| `session_status` | `SessionStatus` | R | the status the session is in *after* this checkpoint |
| `state_hash` | hash | R | §3.5 |
| `state` | `State` | R | the **full** state, inline |

**Full state inline, not a reference [decided here].** The log must be self-sufficient to restore (`restore(checkpoint_hash)` needs nothing but the file), the P1 artifact store is a no-op so a reference could not be dereferenced (D12), and provenance/replay tooling gets one file to copy. Size is bounded by spill (large results are handles) and by compaction (P2.6 trims `messages`). The checkpoint payload is redacted like any other, and `state_hash` was computed over the same redacted content (§4), so restore → rehash verifies.

```rust
pub struct CheckpointPayload { pub turn: u64, pub reason: CheckpointReason, pub session_status: SessionStatus, pub state_hash: Hash, pub state: State }
pub enum CheckpointReason { UserInput, TurnEnd, TaskUpdate, Resume, Recovery, Compaction, Cancel, Failure, End, Suspend }
```
```json
{"seq":14,"ts":"2026-09-06T12:00:08.540Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"checkpoint","payload":{"turn":1,"reason":"turn_end","session_status":"running","state_hash":"b3:8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f8f","state":{"schema_version":1,"session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","created_at":"2026-09-06T12:00:00.001Z","turn":1,"session_status":"running","messages":[{"role":"user","content":[{"type":"text","text":"Mesh the bracket at 2 mm and run the static case."}]},{"role":"assistant","content":[{"type":"text","text":"I'll start the meshing job."},{"type":"tool_use","id":"call_8f2a","name":"run_script","input":{"path":"/work/bracket/mesh.sh","args":["--size","2mm"]}}]},{"role":"tool","content":[{"type":"tool_result","tool_use_id":"call_8f2a","content":{"json":{"task_id":"t1-call_8f2a","status":"running","eta_secs":600,"description":"mesh.sh --size 2mm (pid 41022)"}},"is_error":false}]}],"pending_tasks":{"t1-call_8f2a":{"id":"t1-call_8f2a","tool_use_id":"call_8f2a","tool_name":"run_script","status":"running","started_turn":1,"completed_turn":null,"eta_secs":600,"check_hint":{"pid":41022,"log":"/work/bracket/mesh.log"},"description":"mesh.sh --size 2mm (pid 41022)","outcome":null,"in_process_waker":true}},"profiles":{"model_profile_hash":"b3:1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","agent_profile_hash":"b3:2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b2b","resolved_profile_hash":"b3:3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c","project_profile_hash":null,"bundles_hash":"b3:bdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbdbd"},"memory":null,"notebook_path":"/work/bracket/.grist/notebook.md","sandbox_policy_hash":"b3:4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d","sandbox_backend":"bwrap"}}}
```

### 2.14 `suspended`

After the `checkpoint` that decided `Suspended` (D1 suspension rule) or after `Kernel::suspend`.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `reason` | `"pending_tasks" \| "explicit"` | R | |
| `pending_task_ids` | string[] | R | open tasks, sorted |
| `in_process_wakers` | u32 | R | how many of them are watched by a future in this process |
| `checkpoint_hash` | hash | R | the checkpoint to resume from |

```rust
pub struct SuspendedPayload { pub turn: u64, pub reason: SuspendReason, pub pending_task_ids: Vec<TaskId>, pub in_process_wakers: u32, pub checkpoint_hash: Hash }
pub enum SuspendReason { PendingTasks, Explicit }
```
```json
{"seq":23,"ts":"2026-09-06T12:00:12.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"suspended","payload":{"turn":2,"reason":"pending_tasks","pending_task_ids":["t1-call_8f2a"],"in_process_wakers":1,"checkpoint_hash":"b3:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a"}}
```

### 2.15 `resumed`

Written by `Kernel::open` on a cleanly ended log, or by `Kernel::resume` in-process, before `on_resume` hooks run.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `from_checkpoint_hash` | hash | R | |
| `from_checkpoint_seq` | u64 | R | |
| `from_status` | `SessionStatus` | R | `suspended`, `idle`, or `failed` |
| `cause` | `"task_update" \| "user_message" \| "operator"` | R | `ResumeCause` tag |
| `waker` | `WakerSource` | O, V | when `cause == task_update` |
| `new_process` | bool | R, V | `true` for `Kernel::open` |
| `kernel_version` | string | R, V | |
| `event_schema_version` | u32 | R | the version the resuming kernel writes from here on (§1.3) |

```rust
pub struct ResumedPayload { pub from_checkpoint_hash: Hash, pub from_checkpoint_seq: u64, pub from_status: SessionStatus, pub cause: ResumeCauseKind, pub waker: Option<WakerSource>, pub new_process: bool, pub kernel_version: String, pub event_schema_version: u32 }
pub enum ResumeCauseKind { TaskUpdate, UserMessage, Operator }
```
```json
{"seq":24,"ts":"2026-09-06T12:09:41.100Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"resumed","payload":{"from_checkpoint_hash":"b3:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a","from_checkpoint_seq":22,"from_status":"suspended","cause":"task_update","waker":{"kind":"in_process_exit","trust_tier":null,"detail":{"pid":41022}},"new_process":false,"kernel_version":"0.0.0","event_schema_version":1}}
```

### 2.16 `cancelled` (D15)

One per cancel that had an effect.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `scope` | `"turn" \| "tool" \| "task"` | R | `CancelScope` tag |
| `tool_use_id` | string | O | for `tool`, and for `turn` when a tool was in flight |
| `task_id` | string | O | for `task` |
| `phase` | `"before_model" \| "model_call" \| "after_model" \| "before_tool" \| "tool" \| "after_tool" \| "idle"` | R | where the loop was |
| `signalled` | bool | R | whether a child process received SIGTERM |
| `skipped_tool_use_ids` | string[] | R | calls in this turn that were never started (given synthetic results) |

```rust
pub struct CancelledPayload { pub turn: u64, pub scope: CancelScopeKind, pub tool_use_id: Option<String>, pub task_id: Option<TaskId>, pub phase: LoopPhase, pub signalled: bool, pub skipped_tool_use_ids: Vec<String> }
pub enum CancelScopeKind { Turn, Tool, Task }
pub enum LoopPhase { BeforeModel, ModelCall, AfterModel, BeforeTool, Tool, AfterTool, Idle }
```
```json
{"seq":40,"ts":"2026-09-06T12:15:03.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"cancelled","payload":{"turn":6,"scope":"turn","tool_use_id":"call_c0de","task_id":null,"phase":"tool","signalled":true,"skipped_tool_use_ids":["call_c0df"]}}
```

### 2.17 `turn_failed` (D15)

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `error_class` | string | R | `provider_rate_limited`, `provider_server`, `provider_timeout`, `provider_transport`, `provider_client`, `provider_auth`, `provider_context_too_long`, `provider_invalid_response`, `replay_miss`, `middleware`, `internal`, `log` |
| `attempts` | u32 | R, V | provider attempts made (1 for non-provider classes) |
| `message` | string | R, V | redacted last error |
| `middleware` | string | O | name, for `error_class == middleware` |
| `hook` | string | O | hook name, for `middleware` |

```rust
pub struct TurnFailedPayload { pub turn: u64, pub error_class: String, pub attempts: u32, pub message: String, pub middleware: Option<String>, pub hook: Option<String> }
```
```json
{"seq":36,"ts":"2026-09-06T12:05:40.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"turn_failed","payload":{"turn":4,"error_class":"provider_rate_limited","attempts":5,"message":"429 Too Many Requests","middleware":null,"hook":null}}
```

### 2.18 `session_failed`

After the `checkpoint{reason: failure}` that follows a `turn_failed`.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `cause_seq` | u64 | R, V | seq of the `turn_failed` |
| `error_class` | string | R | copied from `turn_failed` |
| `checkpoint_hash` | hash | R | the intact checkpoint (D15) |
| `resumable` | bool | R | always `true` (D2); reserved for a future non-resumable class |

```rust
pub struct SessionFailedPayload { pub turn: u64, pub cause_seq: u64, pub error_class: String, pub checkpoint_hash: Hash, pub resumable: bool }
```
```json
{"seq":38,"ts":"2026-09-06T12:05:40.010Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"session_failed","payload":{"turn":4,"cause_seq":36,"error_class":"provider_rate_limited","checkpoint_hash":"b3:abababababababababababababababababababababababababababababababab","resumable":true}}
```

### 2.19 `session_ended`

Explicit end only (D2). Last event of a finished session.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `by` | `"user" \| "operator" \| "parent"` | R | who called `end` |
| `checkpoint_hash` | hash | R | final state |
| `cancelled_task_ids` | string[] | R | open tasks cancelled by ending |

```rust
pub struct SessionEndedPayload { pub turn: u64, pub by: EndedBy, pub checkpoint_hash: Hash, pub cancelled_task_ids: Vec<TaskId> }
pub enum EndedBy { User, Operator, Parent }
```
```json
{"seq":88,"ts":"2026-09-06T13:00:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"session_ended","payload":{"turn":12,"by":"user","checkpoint_hash":"b3:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd","cancelled_task_ids":[]}}
```

### 2.20 `recovered` (D15)

Written by `Kernel::open` when the log did not end cleanly (§6.2). Entire kind is volatile for `diff-logs`.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `checkpoint_hash` | hash | R | restored from |
| `checkpoint_seq` | u64 | R | |
| `discarded_seq` | `{from: u64, to: u64}` | O | inclusive range voided; `null` if the checkpoint was the last line |
| `restored_status` | `SessionStatus` | R | |
| `tasks_cancelled` | string[] | R | in-process-waker tasks cancelled by recovery |
| `kernel_version` | string | R | |
| `event_schema_version` | u32 | R | as in `resumed` |

```rust
pub struct RecoveredPayload { pub checkpoint_hash: Hash, pub checkpoint_seq: u64, pub discarded_seq: Option<SeqRange>, pub restored_status: SessionStatus, pub tasks_cancelled: Vec<TaskId>, pub kernel_version: String, pub event_schema_version: u32 }
pub struct SeqRange { pub from: u64, pub to: u64 }
```
```json
{"seq":45,"ts":"2026-09-06T12:20:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"recovered","payload":{"checkpoint_hash":"b3:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef","checkpoint_seq":41,"discarded_seq":{"from":42,"to":44},"restored_status":"running","tasks_cancelled":[],"kernel_version":"0.0.0","event_schema_version":1}}
```

### 2.21 `compaction` (P2.6, payload reserved now)

Kernel-written around the `on_compact` chain.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `strategy` | string | R | from `request_compaction(strategy)` or `Kernel::compact` |
| `before_state_hash` | hash | R | |
| `after_state_hash` | hash | R | |
| `notebook_hash` | hash | O | hash of the notebook file after compaction (P2.6) |
| `messages_before` | u32 | R | |
| `messages_after` | u32 | R | |
| `tokens_before` | u64 | O | last known input token count |
| `summary_artifact` | hash | O | handle of a summary the middleware stored |

```rust
pub struct CompactionPayload { pub turn: u64, pub strategy: String, pub before_state_hash: Hash, pub after_state_hash: Hash, pub notebook_hash: Option<Hash>, pub messages_before: u32, pub messages_after: u32, pub tokens_before: Option<u64>, pub summary_artifact: Option<ArtifactHandle> }
```
```json
{"seq":120,"ts":"2026-09-06T14:00:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"compaction","payload":{"turn":30,"strategy":"notebook","before_state_hash":"b3:0101010101010101010101010101010101010101010101010101010101010101","after_state_hash":"b3:0202020202020202020202020202020202020202020202020202020202020202","notebook_hash":"b3:0303030303030303030303030303030303030303030303030303030303030303","messages_before":96,"messages_after":14,"tokens_before":38120,"summary_artifact":null}}
```

### 2.22 `spawn` (P2.4, reserved)

Emitted by the `spawn` tool through `HookContext::emit`/`ToolContext` in the **parent** log, after the `tool_result` that carries the task handle.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `tool_use_id` | string | R | |
| `task_id` | string | R | the parent-side task |
| `catalog_name` | string | R | |
| `child_session_id` | string | R, V | |
| `child_log_path` | string | R, V | link to the child log |
| `child_profiles` | `ActiveProfiles` | R | |
| `child_tools` | string[] | R | enforcement level 1 record |
| `child_grants` | string[] | R | every atom is `narrower_than` a parent atom (level 3) |

```rust
pub struct SpawnPayload { pub turn: u64, pub tool_use_id: String, pub task_id: TaskId, pub catalog_name: String, pub child_session_id: SessionId, pub child_log_path: PathBuf, pub child_profiles: ActiveProfiles, pub child_tools: Vec<String>, pub child_grants: Vec<Capability> }
```
```json
{"seq":60,"ts":"2026-09-06T12:30:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"spawn","payload":{"turn":8,"tool_use_id":"call_5paw","task_id":"t8-call_5paw","catalog_name":"model-debugger","child_session_id":"s_01J9Z4AAAAAAAAAAAAAAAAAAAA","child_log_path":"/work/bracket/.agent/sessions/s_01J9Z4AAAAAAAAAAAAAAAAAAAA.jsonl","child_profiles":{"model_profile_hash":"b3:1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","agent_profile_hash":"b3:4444444444444444444444444444444444444444444444444444444444444444","resolved_profile_hash":"b3:5555555555555555555555555555555555555555555555555555555555555555"},"child_tools":["read","bash","python"],"child_grants":["fs.rw:/work/bracket/run","fs.ro:/opt/inhouse","tool:bash","tool:python","tool:read"]}}
```

### 2.23 `child_completed` (P2.4, reserved)

In the parent log, when the child's terminal state is observed (normally alongside the `task_update` that completes the spawn task).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `task_id` | string | R | |
| `child_session_id` | string | R, V | |
| `child_status` | `"done" \| "failed"` | R | |
| `child_final_checkpoint_hash` | hash | R | |
| `child_usage` | `Usage` | O | summed over the child's `model_response` events |

```rust
pub struct ChildCompletedPayload { pub turn: u64, pub task_id: TaskId, pub child_session_id: SessionId, pub child_status: SessionStatus, pub child_final_checkpoint_hash: Hash, pub child_usage: Option<Usage> }
```
```json
{"seq":75,"ts":"2026-09-06T12:45:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"child_completed","payload":{"turn":10,"task_id":"t8-call_5paw","child_session_id":"s_01J9Z4AAAAAAAAAAAAAAAAAAAA","child_status":"done","child_final_checkpoint_hash":"b3:6666666666666666666666666666666666666666666666666666666666666666","child_usage":{"input_tokens":52110,"output_tokens":4021,"cache_read_tokens":null,"reasoning_tokens":null}}}
```

### 2.24 `context_usage` (P2.9, reserved)

Emitted by the budget middleware after each `model_response`.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `input_tokens` | u64 | R | from `usage` (or an estimate) |
| `budget_tokens` | u64 | R | `context_budget_tokens`, default 40000 (D16) |
| `over_budget` | bool | R | when `true`, a `warning{class: context_budget_exceeded}` follows |
| `source` | `"provider_usage" \| "estimate"` | R | |

```rust
pub struct ContextUsagePayload { pub turn: u64, pub input_tokens: u64, pub budget_tokens: u64, pub over_budget: bool, pub source: UsageSource }
pub enum UsageSource { ProviderUsage, Estimate }
```
```json
{"seq":16,"ts":"2026-09-06T12:00:08.545Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"context_usage","payload":{"turn":1,"input_tokens":1834,"budget_tokens":40000,"over_budget":false,"source":"provider_usage"}}
```

### 2.25 `harness_edit` (P4, reserved)

Emitted by the evolve loop's tooling when the agent changes a file in the mutable layer. Never by the kernel.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `target` | string | R | repo-relative path of the edited profile/skill/workflow/policy |
| `target_kind` | `"prompt" \| "skill" \| "tool_description" \| "middleware_chain" \| "workflow" \| "memory_module" \| "sandbox_policy" \| "tool_allowlist" \| "tool"` | R | the P4.4 search space |
| `before_hash` | hash | O | `null` when created |
| `after_hash` | hash | R | |
| `rationale_artifact` | hash | O | handle of the written hypothesis (P4.4) |

```rust
pub struct HarnessEditPayload { pub turn: u64, pub target: String, pub target_kind: String, pub before_hash: Option<Hash>, pub after_hash: Hash, pub rationale_artifact: Option<ArtifactHandle> }
```
```json
{"seq":200,"ts":"2026-09-06T15:00:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"harness_edit","payload":{"turn":40,"target":"profiles/agents/model-debugger.toml","target_kind":"tool_allowlist","before_hash":"b3:7777777777777777777777777777777777777777777777777777777777777777","after_hash":"b3:8888888888888888888888888888888888888888888888888888888888888888","rationale_artifact":"b3:9999999999999999999999999999999999999999999999999999999999999999"}}
```

### 2.26 `ask_user` (D17)

Kernel-written before the `ask_user` tool's `Host::ask_user` call.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `tool_use_id` | string | R | |
| `question_id` | string | R | `q<turn>-<tool_use_id>` |
| `question` | string | R | |
| `options` | string[] | R | may be empty |
| `allow_free_text` | bool | R | |

```rust
pub struct AskUserPayload { pub turn: u64, pub tool_use_id: String, pub question_id: String, pub question: String, pub options: Vec<String>, pub allow_free_text: bool }
```
```json
{"seq":50,"ts":"2026-09-06T12:25:00.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"ask_user","payload":{"turn":7,"tool_use_id":"call_a5k1","question_id":"q7-call_a5k1","question":"The bracket has two load cases in the deck. Run both or only LC1?","options":["both","LC1 only"],"allow_free_text":true}}
```

### 2.27 `user_answer` (D17)

Kernel-written when `Host::ask_user` returns, before the corresponding `tool_result`. Provenance attributes it to the human (§7).

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `tool_use_id` | string | R | |
| `question_id` | string | R | |
| `answer` | string | O | `null` when declined |
| `declined` | bool | R | |

```rust
pub struct UserAnswerPayload { pub turn: u64, pub tool_use_id: String, pub question_id: String, pub answer: Option<String>, pub declined: bool }
```
```json
{"seq":51,"ts":"2026-09-06T12:25:41.000Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"user_answer","payload":{"turn":7,"tool_use_id":"call_a5k1","question_id":"q7-call_a5k1","answer":"both","declined":false}}
```

### 2.28 `warning`

Non-fatal conditions. Entire kind is volatile for `diff-logs`.

| Field | Type | R/O | Notes |
|---|---|---|---|
| `turn` | u64 | R | |
| `class` | string | R | see list |
| `message` | string | R | redacted |
| `detail` | any | O | |
| `source` | string | R | `"kernel"` or the emitting middleware/tool name |

Classes defined now: `sandbox_backend_none` (D14; at every `create`/`open`), `artifact_store_noop`, `memory_noop`, `unregistered_tool`, `task_update_ignored`, `task_update_unknown`, `task_update_invalid`, `late_redaction`, `spill_cap_clamped`, `spill_store_failed`, `state_migrated`, `profile_hash_mismatch` (resume under different profiles), `profile_warning` (a `W_*` code from the `profiles` validator; `detail: {code, toml_path}`, `source: "profiles"`), `profile_drift` (P5.1 fine-tune drift; `detail: {expected, actual}`), `context_budget_exceeded` (P2.9), `compaction_request_ignored`, `secret_too_short`. `profile-schema.md` 0.1 refers to these as `warning{class: "profile_warning"}` and `warning{class: "profile_drift"}`; they are `warning` events with these classes, not separate kinds.

```rust
pub struct WarningPayload { pub turn: u64, pub class: String, pub message: String, pub detail: Option<Value>, pub source: String }
```
```json
{"seq":5,"ts":"2026-09-06T12:00:00.009Z","session_id":"s_01J9Z3Q0R5X8K2M4N6P8Q0R2T4","kind":"warning","payload":{"turn":0,"class":"sandbox_backend_none","message":"sandbox backend `none` is in use; tools run WITHOUT isolation (development build)","detail":null,"source":"kernel"}}
```

---

## 3. Hash definitions

All hashes are BLAKE3 (32-byte output) rendered `b3:` + 64 lowercase hex (D3). Unless a definition says `of_bytes`, the input is the RFC 8785 canonical JSON of the stated value (§3.8). Every hash is computed **after** ingress redaction (§4), so recorded and replayed values agree.

### 3.1 `request_hash`

`Hash::of_canonical_json` of the object `{ "model_id", "system", "messages", "tools", "params" }` taken from `ModelRequest`, i.e. `ModelRequest` minus its volatile part. **Volatile list (excluded):** `trace` and everything in it: `session_id`, `turn`, `attempt`, `checkpoint_hash`, `request_id`. Also excluded by construction: HTTP headers, endpoint URL, API key (never in the struct), and wall-clock time. Retries share one `request_hash`.

`system` is the block list (`PromptBlock` with `kind`, `name`, `text`, `hash`), so a change in block *order* changes the hash (D7 fixes the order). `tools` is the ordered `ToolDefinition` list: a description phrasing change or a lazy-exposure difference changes the hash, which is the desired behavior for testing profiles.

### 3.2 `response_hash`

`Hash::of_canonical_json` of `{ "content", "stop_reason", "usage", "model_id" }` from `ModelResponse`. Excluded: `raw_response_hash` (recorded separately), `response_id`, timing. `content` is the post-`after_model` block list (see §2.7 note). `Thinking.signature` is included when present (it round-trips).

### 3.3 `args_hash` and the tool `request_hash`

- `args_hash = Hash::of_canonical_json(&call.input)` — the arguments alone (D13); the provenance projector uses it to identify identical invocations across sessions.
- tool `request_hash = Hash::of_canonical_json(&{ "tool_use_id", "name", "input" })` — the replay key (`ToolCall::request_hash`). Including `tool_use_id` makes two identical calls in one turn distinct cassette entries.

Both are computed after `before_tool` (on the possibly edited call).

### 3.4 `result_hash`

`Hash::of_canonical_json(&{ "content", "is_error" })` from `ToolOutput`, **post-spill and post-redaction**: it is the hash of what entered the context. Excluded: `artifact_handles`, `spilled`, `origin`, `task`, duration. The full pre-spill bytes are addressable through the spill handle (`spill.handle = Hash::of_bytes(bytes)`), so provenance has both.

### 3.5 `state_hash` and the volatile field list (D3)

`state_hash = Hash::of_canonical_json(&state_without_volatile)` where `state_without_volatile` is `State` serialized to JSON with these members **removed** (not nulled):

| Field | Why volatile |
|---|---|
| `session_id` | A replayed or forked session runs under a new id (P3.6 forks; replay into a scratch directory). Recorded in the envelope and `session_created`. |
| `created_at` | Wall clock. Recorded in `session_created`. |
| `sandbox_policy_hash` | Derived from absolute mount paths and the profile's sandbox defaults; differs between the HPC login node and a laptop replay. Recorded in `session_created`, `checkpoint.state`, and every `tool_call.policy_hash`. |
| `sandbox_backend` | `bwrap` in production, `none` on a macOS dev machine (D14); the conversation is the same. Recorded in `session_created`, `checkpoint.state`, and a `warning` when `none`. |

Everything else is hashed: `schema_version`, `turn`, `session_status`, `messages`, `pending_tasks` (including `eta_secs`, `check_hint`, `description`, `outcome`, `in_process_waker`; none of these is wall-clock), `profiles`, `memory`, `notebook_path`.

This list is normative; `State::VOLATILE_FIELDS` in `kernel-interface.md` §3.4 MUST equal it and a P1.1 test asserts that changing any listed field leaves `state_hash` unchanged and changing any other field changes it. **[decided here]** (D3 says "an enumerated list"; this is the enumeration and its justification — see open question 1.)

Why `notebook_path` is *not* volatile: it is chosen by the agent profile, not by the machine, and a fork that changes it is a different configuration. Why `turn` is *not* volatile: two checkpoints with identical messages but different turn counts are different states for resume purposes.

### 3.6 Other hashes in payloads

| Hash | Input |
|---|---|
| `checkpoint_hash` (in `model_request`, `tool_call`, `suspended`, `resumed`, …) | equals the `state_hash` of the referenced `checkpoint` |
| `system_prompt_hash` | canonical JSON of `req.system` (`PromptBlock[]`) |
| `params_hash` | canonical JSON of `req.params` |
| `chain_hash` | canonical JSON of `chain` (`[{index, name, priority, source, config_hash}]`) |
| `policy_hash` | canonical JSON of the tool's `SandboxPolicy` |
| `State.sandbox_policy_hash` | canonical JSON of the session envelope `SandboxPolicy` = `derive_policy_with(grants, grants, limits)` (`kernel-interface.md` §3.12; `profile-schema.md` §7.2 step 9) |
| `PromptBlock.hash` | `Hash::of_bytes(text.as_bytes())` |
| `ArtifactHandle` / `spill.handle` / `raw_response_hash` | `Hash::of_bytes(bytes)` |
| `notebook_hash` | `Hash::of_bytes` of the notebook file |
| profile hashes (`model_profile_hash`, `agent_profile_hash`, `resolved_profile_hash`, `profile_load.hash`) | computed by `profiles` per `profile-schema.md`; opaque `Hash` values here |

### 3.7 Identity of checkpoints

`state_hash` is the checkpoint's identity. Two checkpoints in one log can share a hash only if the state is identical, which the `turn` counter and `session_status` make rare (a `cancel` checkpoint after a cancelled model call differs from the previous `turn_end` checkpoint in `turn` and usually `session_status`). `restore(hash)` picks the **latest** effective checkpoint with that hash; a duplicate is therefore harmless.

### 3.8 Canonicalization (RFC 8785, JCS)

- Canonical JSON is produced by one function, `kernel::canonical_json`, used by every hash and by `diff-logs`. P1.1 chooses the crate (`serde_jcs` is the expected choice; an in-crate implementation over `serde_json::Value` is acceptable if it passes the RFC 8785 test vectors, which P1.1 MUST include).
- JCS rules that matter here: object members sorted by UTF-16 code units of the key (not by bytes: this differs from `BTreeMap<String,_>` ordering for keys outside the BMP, so the canonicalizer MUST NOT rely on `serde_json`'s map ordering); no insignificant whitespace; strings escaped minimally per JCS; numbers serialized per ECMAScript `Number::toString` (so `1.0` canonicalizes to `1`, and `f64` fields such as `temperature` are stable).
- **Non-finite floats are rejected:** `Hash::of_canonical_json` returns `HashError::NotCanonicalizable` for `NaN`/`±Inf` (JSON has no representation and JCS forbids them). The kernel MUST treat such an error in `request_hash`/`state_hash` as a turn failure (`error_class: internal`); a tool result containing a non-finite float is rejected at normalization (step E6) as `ToolError::InvalidInput`, so it never reaches `State`. `serde_json` with default features already refuses to serialize non-finite `f64` (it errors), which gives the same result at a different layer; the test MUST cover both paths.
- Integers above 2⁵³ lose precision under JCS number rules; the kernel uses `u64` for `seq`, `turn`, token counts, and sizes, none of which approach that in practice, but a P1.1 test MUST assert that `u64::MAX` round-trips through the canonicalizer as the RFC specifies (it does not: it is emitted as `18446744073709552000`), and the kernel MUST NOT hash a value whose meaning depends on such an integer. Hash strings, ids, and handles are strings, never integers.
- Byte strings (`ProcessOutput.stdout`, artifact bytes) never enter a hashed JSON structure directly; they are hashed with `of_bytes` and referenced by handle or, when small enough to be inline in a tool result, are already UTF-8 text in a JSON string.

---

## 4. Redaction (D10)

### 4.1 Where

Two passes with one `Redactor` instance (shared by the `SecretResolver`, the kernel, and the log writer):

1. **Ingress (kernel).** Model response content, tool outputs, user messages, task outcomes, and `ask_user` answers are redacted as they enter the kernel, **before** hashing, spill, `after_*` hooks, and appending to `State`. Consequence: the model sees the redacted text, `State` never contains a raw secret, and every hash in the log is a hash of redacted content.
2. **Writer (log).** `EventLog::append` runs the same redactor over the whole `payload` before serializing the line. If this pass changes anything, the writer appends `warning{class: "late_redaction", detail: {seq, replacements}}` right after the offending line. The line is still written redacted; the warning marks a kernel bug (an ingress path was missed) or a secret registered after ingress happened.

Redaction never touches envelope fields or object keys; only JSON string values (recursively through arrays and objects). Numbers and booleans are never secrets by construction.

### 4.2 Replacement text

`[REDACTED:<kind>]`, where `<kind>` is one of the table below. The replacement is applied to the matched span only; surrounding text is kept (so `Authorization: Bearer abc…` becomes `Authorization: Bearer [REDACTED:bearer]`). Multiple matches in one string are each replaced. The replacement is deterministic (no counters, no per-run salt), so record and replay agree.

### 4.3 Sources of known secrets

- **Resolved secret values.** `SecretResolver::resolve_secret` MUST call `Redactor::register_secret` with every value it returns, before returning it. Provider clients therefore cannot obtain a key the redactor does not know. Values shorter than 8 bytes are not registered (too many false positives) and produce `warning{class: "secret_too_short"}` once per name.
- **Launcher-supplied values.** The launcher MAY pre-register values it knows (e.g. everything under a `secrets/` directory it mounted) before constructing the kernel.
- **Known values are matched as exact substrings** (byte-wise, case-sensitive) and replaced with `[REDACTED:secret]`. Known-value replacement runs **before** pattern matching, so a key that also matches a pattern is reported as `secret`.

### 4.4 Built-in patterns (normative minimum)

Compiled once with the `regex` crate; extendable via `Redactor::with_extra_patterns` (the launcher may add site-specific ones from the profile; `profile-schema.md` names the key). Order of application is the table order.

| kind | Pattern (Rust `regex` syntax) | Matched span replaced |
|---|---|---|
| `pem` | `-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----` | whole block |
| `jwt` | `\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\b` | whole token |
| `bearer` | `(?i)\bbearer\s+([A-Za-z0-9\-._~+/]{16,}=*)` | group 1 (the token after `Bearer `) |
| `api_key` | `\bsk-(?:[A-Za-z0-9_-]{2,}-)?[A-Za-z0-9_-]{16,}\b` | whole token (OpenAI/Anthropic/LiteLLM style, incl. `sk-ant-…`, `sk-proj-…`) |
| `api_key` | `\bgh[pousr]_[A-Za-z0-9]{36,}\b` | GitHub tokens |
| `api_key` | `\bxox[baprs]-[A-Za-z0-9-]{10,}\b` | Slack tokens |
| `api_key` | `\bAIza[0-9A-Za-z_-]{35}\b` | Google API keys |
| `aws_key` | `\b(?:AKIA\|ASIA)[0-9A-Z]{16}\b` | AWS access key id |
| `aws_key` | `(?i)\baws_secret_access_key\b\s*[:=]\s*["']?([A-Za-z0-9/+=]{40})` | group 1 |
| `basic_auth` | `(?i)\bbasic\s+([A-Za-z0-9+/]{16,}={0,2})\b` | group 1 |
| `url_credential` | `://([^/\s:@]+):([^/\s@]+)@` | group 2 (the password in `scheme://user:pass@host`) |
| `env_secret` | `(?i)\b([A-Z0-9_]*(?:API_KEY\|SECRET\|TOKEN\|PASSWORD\|PASSWD\|CREDENTIALS?)[A-Z0-9_]*)\s*=\s*["']?([^\s"']{8,})` | group 2 (value in `NAME=value` dumps such as `env` output) |

Notes: `(?i)` is inline case-insensitivity; the `\|` in the table is a literal `|` alternation escaped for Markdown. P1.3 MUST ship a test corpus with at least one positive and one near-miss per row (e.g. `sk-` followed by 8 chars is not redacted; `Bearer` alone is not redacted), plus the D10 acceptance test: a registered secret value placed in a tool result never appears in the log file.

### 4.5 Hashes after redaction

Because ingress redaction precedes hashing, `request_hash`, `response_hash`, `args_hash`, `result_hash`, and `state_hash` are all hashes of redacted content. Replay feeds the redacted cassette content back through the same redactor (a fixed point: redacting already-redacted text changes nothing, which a P1.3 test asserts), so replayed hashes equal recorded ones. A secret registered *later* in a session (a provider client resolving a second key mid-run) can produce a `late_redaction` on payloads that were hashed before registration; `diff-logs` treats the affected line as a divergence, which is correct: the recorded content leaked.

---

## 5. Record/replay mapping (D16)

### 5.1 The cassette is the log

No separate recording store is needed. `Cassette::from_log(reader)` builds the P1.4 cassette from the effective events (§6.1):

| Cassette part | Source events | Key |
|---|---|---|
| `model[(checkpoint_hash, request_hash)] → ModelResponse` | `model_request` (key) + the following `model_response` with the same `request_hash` (value: `content`, `stop_reason`, `usage`, `model_id`, `raw_response_hash`) | `(model_request.checkpoint_hash, model_request.request_hash)` |
| `tools[(checkpoint_hash, request_hash)] → ToolOutput` | `tool_call` (key) + the following `tool_result` with the same `tool_use_id` (value: `content`, `is_error`, `artifact_handles`, `spilled`, `task`, `origin`) | `(tool_call.checkpoint_hash, tool_call.request_hash)` |
| `inputs` (ordered) | `user_message` → `ReplayInput::UserMessage{applied, message}`; `task_update` with `applied.at != ignored` → `ReplayInput::TaskUpdate{applied, update}` (waker `kind` rewritten to `"replay"`, which is why `waker` is volatile); `user_answer` → `ReplayInput::UserAnswer` | log order |

A `model_request` without a matching `model_response` (crash, failure) contributes nothing; replaying up to that point then stops with `ReplayError::EarlyStop` or reproduces the failure if the failure was a replay-independent class (`middleware`).

The `Recorder` middleware builds the same structure in memory during a live run; P1.4 asserts equality with `Cassette::from_log` after every loop test, which is how the log's completeness as a cassette is itself tested.

### 5.2 The key

`(checkpoint_hash, request_hash)`. `checkpoint_hash` is the `state_hash` of the most recent `checkpoint` when the request was built (kernel invariant 1 in `kernel-interface.md` §6), stamped into `ModelRequest.trace.checkpoint_hash` and `HookContext.checkpoint_hash`. `request_hash` is §3.1 for models and §3.3 for tools. On replay, the `ReplayProvider`/`ReplayTool` compute the same two values from the live (replayed) state and look them up; a miss is an error (`ProviderError::ReplayMiss` / `ToolError::ReplayMiss`), never a live call (P1.4).

Because the volatile fields (§3.5) are excluded from `checkpoint_hash`, a replay under a different `session_id`, machine, or sandbox backend hits the same keys.

### 5.3 What `diff-logs` compares and strips

`diff_logs(recorded, replayed)` compares the two **effective** logs (§6.1) as ordered sequences of `(kind, payload)` after:

1. Dropping every envelope field (`seq`, `ts`, `session_id`). Order is the sequence order, so `seq` is implied.
2. Dropping every event whose kind is entirely volatile: `provider_retry`, `warning`, `recovered`.
3. Removing the fields marked **V** in §2 from the remaining payloads. Consolidated list:

| Kind | Stripped fields |
|---|---|
| `log_opened` | `kernel_version`, `mode` |
| `session_created` | `session_id`, `created_at`, `kernel_version`, `sandbox_backend`, `sandbox_policy_hash`, `artifact_store`, `memory`, `provider` |
| `profile_load` | `path` |
| `model_response` | `attempts` |
| `tool_call` | `policy_hash` |
| `tool_result` | `duration_ms` |
| `task_update` | `waker` |
| `checkpoint` | `state.session_id`, `state.created_at`, `state.sandbox_policy_hash`, `state.sandbox_backend` |
| `resumed` | `waker`, `new_process`, `kernel_version` |
| `turn_failed` | `attempts`, `message` |
| `session_failed` | `cause_seq` |
| `spawn` | `child_session_id`, `child_log_path` |
| `child_completed` | `child_session_id` |

4. Serializing each remaining payload with `canonical_json` and comparing bytes.

**Byte-identical payload (D16)** means: for every index `i`, `kind_i` is equal and `canonical_json(strip(payload_i))` is byte-for-byte equal between the recorded and replayed sequences, and the sequences have the same length. `DiffReport.first_diff` names the first index that violates this. Nothing else is tolerated: not field reordering (canonicalization removes it), not whitespace, not float formatting (JCS fixes it).

Why `checkpoint.state` is compared at all (rather than only `state_hash`): the hash already excludes the volatile fields, so comparing the stripped state is redundant with comparing `state_hash`, but it produces a readable first divergence instead of "hash differs".

### 5.4 Replay-mode log

A kernel driven by `ReplayDriver` writes an ordinary log (`log_opened.mode = "replay"`) into a directory the driver chooses. It is a complete log in its own right: it can be restored from, projected, and diffed. It is never appended to the recorded file.

---

## 6. Checkpoint, restore, recovery, migration

### 6.1 The effective log

The physical file may contain lines that a later `recovered` event voided. Readers construct the **effective log** as: all events, minus every event whose `seq` falls in a `discarded_seq` range of any `recovered` event, minus any `recovered` event's own predecessor ranges transitively (a recovery after a recovery discards a range that may itself contain a `recovered` line; that line and its range are simply inside the newer range). `EventLogReader::effective()` yields exactly this. Every consumer except forensic tooling (`iter()`) reads the effective log: `restore`, `Cassette::from_log`, `diff-logs`, the provenance projector.

### 6.2 `restore(checkpoint_hash)` and crash recovery

`restore` needs: the effective log; the `MigrationRegistry`; nothing else. Algorithm:

1. Scan the effective log backwards for the latest `checkpoint` with `payload.state_hash == checkpoint_hash` (`RestoreError::NotFound` otherwise).
2. Take `payload.state` as raw JSON. Recompute `state_hash` over it (minus volatile fields) **before** any migration and compare with `payload.state_hash` (`RestoreError::HashMismatch` on failure: the line was altered or a late redaction changed it; this is never silently accepted).
3. If `state.schema_version < STATE_SCHEMA_VERSION`, run `MigrationRegistry::migrate_to_current` (§6.3). If greater, `MigrationError::NewerThanSupported`.
4. Deserialize into `State`. Return `Migrated { state, migrated }`.

Crash recovery (`kernel-interface.md` §7.3) is `restore_latest` plus the `recovered` event and the in-process-waker task cancellation. The launcher distinguishes "resume" from "recover" only by the log's last effective event: `suspended`, `session_ended`, `session_failed`, or a `checkpoint` with `session_status: idle` means clean; anything else means the previous process died and `recovered` is written.

What a client should assume: a user message is durably part of the session once a `checkpoint` event follows its `user_message` (that checkpoint is written immediately when the message is applied from `Idle`, or at the end of the turn when it was queued). The protocol server (P1.9) SHOULD acknowledge user input on that checkpoint, not on receipt.

### 6.3 Migration path for older `State.schema_version`

- Each bump of `STATE_SCHEMA_VERSION` ships a `StateMigration` for `from_version = old`. The registry chains `v → v+1 → … → current` on the raw JSON. A missing step fails loudly (`MigrationError::MissingStep`); there is no lenient deserialization.
- After a migration the kernel logs `warning{class: "state_migrated", detail: {from, to}}` and writes a fresh `checkpoint{reason: resume | recovery}` whose `state.schema_version` is current, so the log never needs the migration twice for the same session.
- `state_hash` after migration differs from before (the content changed); the new checkpoint carries the new hash. Replay keys recorded under the old schema will therefore miss after a migration — acceptable, and flagged: a migrated session is not replayable against its pre-migration cassette unless the migration is the identity on the hashed fields (P1 exit requires "migrations exercised by at least one test"; that test SHOULD include a hash-preserving migration and a hash-changing one).
- `checkpoint` payloads of old versions remain readable by old kernels; the event schema version (§1.3) is orthogonal.

---

## 7. Provenance mapping hint (P3.1)

Short map from event kinds to W3C PROV-DM. The projector (`provenance` crate) owns the real schema; this table is what the D13 fields were designed to feed. Agent identity = `(model_id, model_profile_hash, agent_profile_hash, kernel_version)` from `session_created` + `model_request.profiles`.

| Event kind | PROV noun | Edges |
|---|---|---|
| `session_created`, `profile_load` | `Agent` (the configured agent), `Entity` (each profile, by hash) | agent `wasDerivedFrom` previous agent when profile hashes change (§10 rule 2 of the dev plan) |
| `model_request` / `model_response` | `Activity` (model call), `Entity` (request by `request_hash`, response by `response_hash`) | `used` request, `generated` response, `wasAssociatedWith` agent, `wasInformedBy` previous checkpoint |
| `tool_call` / `tool_result` | `Activity` (tool call), `Entity` (args by `args_hash`, result by `result_hash`, each artifact handle) | `used` args, `generated` result and artifacts, `wasInformedBy` the model call that emitted the `tool_use` |
| `task_started` / `task_update` | `Activity` (external job; long-running), `Entity` (outcome) | `wasStartedBy` the tool call, `generated` outcome, waker `kind` as an attribute |
| `checkpoint` | `Entity` (state by `state_hash`) | `wasGeneratedBy` the turn's activities; `wasDerivedFrom` previous checkpoint |
| `compaction` | `Activity` | `used` before-state, `generated` after-state and notebook entity |
| `spawn` / `child_completed` | `Activity` (delegation), `Agent` (child) | child agent `actedOnBehalfOf` parent; child log linked by `child_session_id` |
| `user_message`, `user_answer` | `Entity` attributed to the human `Agent` | `wasAttributedTo` human |
| `harness_edit` | `Activity`; new profile `Entity` | new entity `wasDerivedFrom` old (`before_hash` → `after_hash`), `wasAttributedTo` the proposing agent |
| `ask_user` | `Activity` (interaction) | `wasInformedBy` the tool call |
| `cancelled`, `turn_failed`, `session_failed`, `recovered` | attributes on the enclosing activity/session | — |
| `log_opened`, `middleware_chain_resolved`, `suspended`, `resumed`, `session_ended`, `context_usage`, `warning`, `provider_retry` | not projected as nodes; `middleware_chain_resolved.chain_hash` becomes an attribute of the agent | — |

The agent has no write path into this graph: extensions can emit only the closed `ExtensionEvent` set, and the projector ignores any agent-authored content that claims pedigree (P3 exit criterion).

---

## 8. Open questions for the reviewer

1. **Volatile state fields** (§3.5): `session_id`, `created_at`, `sandbox_policy_hash`, `sandbox_backend`. Confirm the set, especially `sandbox_policy_hash` (excluded for replay portability across machines; still logged everywhere it matters).
2. **`tool_result` carries the post-spill content** (§2.10) and `model_response` carries full content blocks (§2.7), making the log the complete cassette (§5.1) at a cost of at most `spill_cap_bytes` per tool call. Alternative: hashes only in the log plus a separate cassette file written by the `Recorder`.
3. **`model_response` is written after the `after_model` chain** (§2.7) so parsed tool calls appear in `content` and `response_hash` covers the post-parse blocks; the raw provider output is represented only by `raw_response_hash`. Alternative: log both raw and parsed content (doubles the size of every response event).
4. **`user_message` and `task_update` are logged at application time, not receipt time** (§2.5, §2.12), with an `applied` marker for the replay driver. Receipt timing is thereby lost from the log (the protocol server may log it on its side).
5. **Full `State` inline in every checkpoint** (§2.13). Confirm for P1 and as the default after P2.5.
6. **Whole-kind volatility of `warning`** in `diff-logs` (§5.3). A `warning` such as `context_budget_exceeded` is semantically meaningful, but replay across dev/prod backends emits different `sandbox_backend_none`/`artifact_store_noop` warnings; per-class stripping would be more precise at the cost of a class list in the diff tool.
7. **`model_request` logs a summary, not the request** (§2.6). The full request is reconstructable from the previous checkpoint plus the chain; the projector gets `request_hash`, `system_prompt_hash`, `params_hash`, `tool_names`. Confirm that no consumer needs the literal request text in the log.
8. **Redaction on ingress changes what the model sees** (§4.1). A tool that legitimately prints a token (e.g. `cat .env`) shows the model `[REDACTED:env_secret]`. This is the intended D10 posture, but it is a behavior change relative to "writer-only" redaction.
9. **Built-in pattern list** (§4.4) — the normative minimum. Anything site-specific to add now (e.g. the HPC site's Slurm token format, Kerberos tickets)?
10. **Appending to the same file across an event-schema bump** (§1.3) with the new version stamped in `resumed`/`recovered`, versus starting a new file per version.
11. **`checkpoint.state` compared field-wise by `diff-logs`** even though `state_hash` covers it (§5.3): kept for readable diffs; costs time on long sessions.
12. **Acknowledging user input on the following `checkpoint`** (§6.2) rather than on receipt, given D15's "discard events after the last checkpoint".
13. **`recovered` cancels tasks with lost in-process wakers** (`tasks_cancelled`, §2.20) — same question as `kernel-interface.md` §10 item 9, listed here because it changes what the log says happened to the task.
