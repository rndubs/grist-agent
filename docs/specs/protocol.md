# Protocol: ACP with the `_grist/*` extension namespace

**Status:** v0.1, normative for P1.9 (ADR-0004). Companion to `event-schema.md` (the log is the
source of truth; this protocol is a projection of it) and `kernel-interface.md` §3.13–§3.15.

## 1. Shape

- **Wire format:** the Agent Client Protocol, JSON-RPC 2.0, newline-delimited UTF-8, exactly as
  the `agent-client-protocol` Rust SDK 2.1.0 speaks it. **Protocol version 1** is served. The
  SDK keeps ACP v2 behind its `unstable_protocol_v2` feature and the official clients negotiate
  v1, so per ADR-0004 resolution 1 the v2 additions (`session/resume` with `replayFrom`,
  required message ids, the idle state update) are a follow-on; nothing in `_grist/*` depends
  on the version.
- **Two legs, one implementation** (D4): `grist-kernel` serves **one session over stdio**;
  `grist-daemon` serves many sessions over a **unix socket** by spawning one `grist-kernel` per
  session and forwarding every message typed and unchanged. A client that can only spawn a
  subprocess (every editor) spawns `grist-connect`, which pumps its stdio to the socket.
- **Auth** (D11): the socket is mode `0600` in a directory the daemon creates as `0700`, and
  the daemon accepts a connection only when the peer's uid (`SO_PEERCRED` /
  `LOCAL_PEERCRED`) is its own. There is no authentication in the message layer. Remote use
  is an ssh-forwarded socket (`ssh -L`), see `docs/clients/zed.md`.
- **Permission is never asked** (D17): `session/request_permission` is never sent; sandbox
  policy already answered. `fs/*` and `terminal/*` client capabilities are never used (the
  kernel's tools go through `Host` and the sandbox backend, D5/D14); an editor therefore sees
  files as they are on disk, not its unsaved buffers.

## 2. ACP methods and what they do

| Method | Direction | Effect | Log |
|---|---|---|---|
| `initialize` | client → agent | answers `protocolVersion: 1`, `agentCapabilities.loadSession: true`, session capabilities `resume`, `close`, `list`, `agentInfo{name: "grist"}` | — |
| `session/new{cwd, _meta.grist?}` | client → agent | resolves the catalog entry (`_meta.grist.agent`, default `default`) for `cwd` with layer-4 `_meta.grist.profile_overrides` (`profile-schema.md` §4.4), creates `<state>/sessions/<id>.jsonl` and the session record `<id>.toml`, `Kernel::create` | `log_opened`, `session_created`, `profile_load`*, `middleware_chain_resolved` |
| `session/prompt{sessionId, prompt}` | client → agent | the text blocks (and resource-link URIs) joined become one user message; the kernel runs until `Idle`, `Done` or `Failed`, driving through any suspension whose waker lives in the process (`run_script`); the response is the turn's `stopReason` | `user_message` … `checkpoint` |
| `session/cancel{sessionId}` | client → agent (notification) | `KernelHandle::cancel(Turn)` (D15); the prompt returns `stopReason: cancelled` | `cancelled{scope: turn}` |
| `session/close{sessionId}` | client → agent | `Kernel::end` (D2); terminal | `checkpoint`, `session_ended` |
| `session/list` | client → agent | every session record under `<state>/sessions` (`cwd`, `title: "<agent> · <cwd>"`, `updatedAt` = creation) | — |
| `session/load{sessionId, cwd}` | client → agent | reopens the session from its record and log (`Kernel::open`, `ResumeCause::Operator`) and **replays its history** as `session/update` notifications before answering | `resumed` (or `recovered`) |
| `session/resume{sessionId, cwd}` | client → agent | as `session/load` without the replay | `resumed` (or `recovered`) |
| `session/update{sessionId, update}` | agent → client (notification) | §4 | derived |
| `elicitation/create` | agent → client | the `ask_user` tool's question (D17): message = question (+ options when free text is allowed), form schema `{answer: string}` with `enum` = options when free text is not allowed; `accept.content.answer` is the answer, `decline`/`cancel` declines | `ask_user`, `user_answer` |

Errors carry `data.grist`. A turn that ends `Failed` (provider exhaustion, D15) is a JSON-RPC
error with `data.grist = {status: "failed", error_class, resumable: true}`: the checkpoint is
intact and the next `session/prompt` resumes the session (D2). `grist-kernel` refuses a second
`session/new` on its connection (`invalid_request`, "exactly one session").

## 3. `_grist/*` (stock clients ignore all of it)

| Method | Direction | Params → result |
|---|---|---|
| `_grist/subscribe` | client → agent | `{sessionId, kinds?: [event kind]}` → `{}`. From then on every event the log writes for that session is sent as `_grist/event`; `kinds` is a bandwidth filter (empty or absent = all), **not** a security control (ADR-0004 resolution 4: the socket is the boundary). |
| `_grist/unsubscribe` | client → agent | `{sessionId}` → `{}` |
| `_grist/event` | agent → client (notification) | `{sessionId, event}` where `event` is the redacted `Event` verbatim, envelope included (`event-schema.md` §1.1: `seq`, `ts`, `session_id`, `kind`, `payload`). This is how the twelve log-facing kinds with no ACP counterpart reach a client. |
| `_grist/cancel` | client → agent | `{sessionId, scope}` → `{}` with `scope` a `CancelScope` (`{"scope":"turn"}`, `{"scope":"tool","tool_use_id"}`, `{"scope":"task","task_id"}`), `kernel-interface.md` §3.13 |
| `_grist/status` | client → agent | `{sessionId}` → `{status, turn, pendingTaskIds, inProcessWakers, logPath}` from the kernel's `State` |

Method names begin with `_` as ACP requires for extensions. `_meta.grist` on `session/new`
carries `{agent?, profile_overrides?}`. No other `_meta` is read.

## 4. Projection: `Event` → `session/update`

Pure and total (`crates/orchestrator/src/acp/project.rs`, `updates_for`); every update is
derived from an event or a streaming delta the kernel also logs. Nothing here is state.

| Event / delta | `sessionUpdate` |
|---|---|
| `user_message` text blocks | `user_message_chunk` (on `session/load` replay) |
| `ModelDelta::TextDelta` / `ThinkingDelta` | `agent_message_chunk` / `agent_thought_chunk` as they stream |
| `model_response` text / thinking blocks | the same chunks, **only if nothing streamed since the last `model_request`** (scripted and non-streaming providers) |
| `tool_call` | `tool_call{toolCallId: tool_use_id, title: "<name>: <path|command|code|question>", kind (read→read, write/edit→edit, bash/run_script/python→execute, else other), status: in_progress, rawInput: input}` |
| `tool_result` | `tool_call_update{status: completed|failed, rawOutput: content, content: [text] for text blocks}` |
| `task_update` (terminal) | `tool_call_update{toolCallId: "task:<task_id>", status: completed|failed, rawOutput: payload}` |
| everything else (`checkpoint`, `profile_load`, `middleware_chain_resolved`, `provider_retry`, `task_started`, `suspended`, `resumed`, `recovered`, `cancelled`, `turn_failed`, `session_failed`, `compaction`, `context_usage`, `spawn`, `child_completed`, `harness_edit`, `warning`, `log_opened`, `session_created`, `session_ended`, `ask_user`, `user_answer`) | nothing; `_grist/event` only |

Suspension inside a prompt (a `run_script` task, D1) is invisible to a stock client except as a
pause: the log shows `suspended` → `task_update` → the next `model_request`, and the prompt
returns when the session is idle again. A suspension on an *external* waker (none before P3.4)
returns `stopReason: end_turn` and leaves the session `suspended`; `_grist/status` says so.

## 5. Processes and state

- **State directory** `GRIST_STATE_DIR` (default `~/.grist`): `sessions/<id>.jsonl` (the log),
  `sessions/<id>.toml` (`SessionRecord{session_id, workdir, agent, overrides, created_at}`,
  what `session/load` needs to rebuild the same `KernelConfig`), `daemon.sock`.
- **Launcher** (`crates/orchestrator/src/launcher.rs`): `profiles::resolve` against the compiled
  registry (six base tools + `ask_user`), backend from `sandbox_backend` (`bwrap`; `none` only
  in a `dev-sandbox-none` build, D14; `GRIST_SANDBOX_BACKEND` overrides for development),
  endpoint from `GRIST_ENDPOINT_<NAME>_URL`, the OpenAI-compatible client from the model
  profile's quirks with the bearer secret as a handle (D10), model-profile tool descriptions by
  wrapping the compiled tools' definitions.
- **Daemon lifetime rule:** sessions belong to the connection that opened them; when it closes
  their kernel processes are killed. The log has every checkpoint, so `session/load` on a new
  connection continues where the log ends (`kernel-interface.md` §7.3 if the log was torn).
- **Kernel process lifetime:** `grist-kernel` exits when its stdin closes. While a prompt is
  suspended on an in-process waker the process is inside `session/prompt` and stays alive by
  construction (`Suspension.in_process_wakers`, §3.15).
