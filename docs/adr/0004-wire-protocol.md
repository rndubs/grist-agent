# ADR-0004: Wire protocol for the daemon and the first client

- **Status:** accepted (2026-09-06). Drafted as `proposed`; the maintainer read the draft, stated no
  strong preference on the four open questions, and delegated them to the author, who resolved
  them as recorded below. Nothing is implemented yet — P1.9 does that.
- **Date:** 2026-09-06
- **Milestone:** P1.9 (see `docs/IMPLEMENTATION_PLAN.md`); resolves open question §15.2 of the dev plan

## Context

P1.9 puts a protocol server in front of the kernel and gives the system its first
client. The wire format has been carried as an open question since v0.1 of the dev
plan (§15.2): **adopt the Agent Client Protocol (ACP), extend it, or define our own
JSON-RPC schema and provide an ACP shim.** It has to be settled before P1.9 starts,
because it determines what the supervisor speaks, what the client is, and how much
of P1.9 is client work rather than daemon work.

Six things constrain the answer. They are not negotiable in this ADR:

- **D4 — process model.** One kernel process serves one session over stdio. The P1.9
  daemon is a *supervisor* that spawns a kernel process per session and proxies a unix
  socket to it; it is the seed of the P3 orchestrator.
- **D11 — transport and auth.** Unix socket, file permissions plus a peer-credential
  check. Remote access is an SSH-forwarded unix socket to the HPC login node.
  Websocket auth is backlogged pending the security team.
- **D15 — cancel.** A protocol message sets a token; the loop notices between hooks;
  the sandbox launcher SIGTERMs a running tool; the cancellation is logged.
  `CancelScope` has three granularities: `Turn`, `Tool{tool_use_id}`, `Task{task_id}`
  (`kernel-interface.md` §3.13).
- **D17 — `ask_user`.** The Host's prompt capability is a *tool*; the answer is logged
  and attributed to the human. **Permission is never asked** — sandbox policy already
  answered that question.
- **The event schema is the state.** Every protocol message must map to or from an
  `Event` (`docs/specs/event-schema.md`, 28 kinds); there is no protocol-only state.
  The protocol is a projection of the log, never a second source of truth.
- **The client requirement.** Windows, macOS and Linux (D14), and **no CLI**
  (dev plan §13, "CLI-less"). The kernel itself is deployed only on Linux.

Two further boundaries apply. The protocol server lives in `orchestrator`/`ext`, never
in `kernel` (CONTRIBUTING rule 1); and the kernel already exposes exactly the surface a
protocol server needs — `KernelHandle::{enqueue_user_message, deliver_task_update,
cancel, subscribe, subscribe_deltas}` plus `KernelConfig.delta_sink`
(`kernel-interface.md` §3.15). No kernel change is proposed here.

There is no spike for this decision; the evidence is the ACP specification as published
on 2026-09-06 (sources at the end), the event schema, and the launcher shape that P1.8
left behind in `crates/orchestrator/tests/launcher.rs`.

## What ACP is, as of 2026-09-06

- JSON-RPC 2.0 between a **client** (editor/UI) and an **agent**. Two protocol
  versions are published: **v1** and **v2**; the Rust SDK `agent-client-protocol` and
  the TypeScript `@agentclientprotocol/sdk` are at 1.0.0.
- **Transport.** stdio is the only stable transport: "the client launches the agent as
  a subprocess", messages are newline-delimited UTF-8 JSON-RPC and "MUST NOT contain
  embedded newlines". Streamable HTTP is a draft. The spec explicitly allows custom
  transports: agents and clients "MAY implement additional custom transport
  mechanisms" as long as the JSON-RPC framing holds.
- **Agent methods** (client → agent): `initialize`, `auth/login` / `auth/logout`,
  `session/new`, `session/resume`, `session/close`, `session/list`, `session/prompt`;
  `session/cancel` is a notification.
- **Client methods** (agent → client): `session/request_permission`,
  `elicitation/create`, and the optional `fs/read_text_file`, `fs/write_text_file`,
  `terminal/create`, `terminal/output`, `terminal/wait_for_exit`, `terminal/kill`,
  `terminal/release`. `session/update` is the agent → client notification carrying
  message chunks, thought chunks, tool calls and tool-call updates, plans, usage and
  mode/state changes; v2 adds required message ids so a message can be replaced, and
  an **idle** state update meaning "ready for a new prompt".
- **Turn shape.** `session/prompt{sessionId, prompt: ContentBlock[]}` streams
  `session/update` notifications and returns a `stopReason`: `end_turn`, `max_tokens`,
  `max_turn_requests`, `refusal`, `cancelled`. `session/cancel` makes the agent stop
  the model call and its tools and return `cancelled` rather than an error.
- **Sessions.** `session/new{cwd, mcpServers}` → `{sessionId}`; v2 `session/resume`
  takes `{sessionId, cwd, mcpServers, replayFrom}` and, with `replayFrom:{type:"start"}`,
  replays the whole conversation as `session/update` notifications before answering.
- **Ecosystem.** Official clients include Zed (macOS/Linux/Windows), JetBrains IDEs,
  Qt Creator and several desktop apps, alongside 100+ community clients across
  editors, desktop, web and mobile.

Field-level details of the v2 schema (exact `sessionUpdate` discriminators, the shape
of the idle state update, `_meta` extension slots) were not verified line by line for
this draft and are the first thing P1.9 should check against the published schema.

## How ACP scores against the constraints

| Constraint | ACP fit | Note |
|---|---|---|
| D4: kernel process per session, stdio | **strong** | ACP's native deployment *is* "client launches the agent as a subprocess and talks newline-JSON-RPC over stdin/stdout". The kernel is an ACP agent; the supervisor is its client. |
| D4: supervisor multiplexes sessions | **good** | ACP sessions are ids in a single connection (`session/new`, `session/list`, `session/close`). The supervisor is an ACP *agent* to the outside and an ACP *client* to each kernel; it owns the id → process map. |
| D11: unix socket, peer creds, SSH | **partial** | Not a defined ACP transport, but explicitly permitted as a custom one; the framing is unchanged. Off-the-shelf clients only know how to spawn a subprocess, so they spawn a ~200-line forwarder that pipes stdio to the socket (locally, or through `ssh -W`-style forwarding). Auth stays where D11 put it: socket permissions + `SO_PEERCRED`/`LOCAL_PEERCRED`, checked by the supervisor, never in the message layer. |
| D15: cancel | **partial** | `session/cancel` covers `CancelScope::Turn` exactly. `Tool{tool_use_id}` and `Task{task_id}` have no ACP counterpart and need an extension method; a stock client simply cannot express them. |
| D17: `ask_user` | **good, with a trap** | `elicitation/create` is the right carrier for `{question, options, allow_free_text}` → answer/declined. The trap is `session/request_permission`, whose whole premise ("ask before running this tool") contradicts D17 and D5/D9. We simply never send it; a client that expects it must tolerate its absence. |
| Every message maps to an `Event` | **partial** | The turn-facing half maps cleanly (below). The log-facing half — checkpoints, profile loads, middleware chains, provider retries, recovery, suspension — has no ACP vocabulary and needs an extension notification. |
| Client on Windows/macOS/Linux, no CLI | **decisive** | Zed and the JetBrains IDEs are official ACP clients on all three platforms. P1.9's "first client" becomes a configuration file instead of an application. |

### Event mapping (the D20 test)

| `Event` kind | ACP message | Direction |
|---|---|---|
| `user_message` | `session/prompt` params | client → agent |
| `model_response` (text) | `session/update` agent message chunk; `subscribe_deltas` feeds it | agent → client |
| `model_response` (thinking) | `session/update` agent thought chunk (`ContentBlock::Thinking`, P1.5) | agent → client |
| `tool_call` | `session/update` tool call, status pending/in progress | agent → client |
| `tool_result` | `session/update` tool call update, status completed/failed | agent → client |
| `ask_user` / `user_answer` | `elicitation/create` request / result | agent → client / client → agent |
| `cancelled` | `session/cancel` notification, then `stopReason: "cancelled"` | client → agent |
| `session_created` | `session/new` result (`sessionId` = our `SessionId`) | agent → client |
| `session_ended` | `session/close` | client → agent |
| replay of a log | `session/resume` with `replayFrom` → `session/update` stream | both |
| `task_started` / `task_update` (D1) | tool call update + the v2 idle state update; **imperfect** — a `Task` outlives a turn, which ACP has no first-class word for | agent → client |
| `suspended` / `resumed` | none; closest is "idle with open tasks" | — |
| `turn_failed` / `session_failed` | JSON-RPC error or `stopReason: "refusal"`; **imperfect** — neither conveys "failed with the checkpoint intact and resumable" (D15) | agent → client |
| `checkpoint`, `log_opened`, `profile_load`, `middleware_chain_resolved`, `provider_retry`, `recovered`, `compaction`, `context_usage`, `spawn`, `child_completed`, `harness_edit`, `warning` | none | — |

Twelve of 28 kinds have no ACP counterpart. They are exactly the kinds a *provenance*
reader wants and a chat UI does not, which is the honest summary of the gap: ACP is a
complete vocabulary for a turn and an incomplete one for a session's record.

### Where ACP's model diverges from ours

- **Who owns the filesystem.** ACP assumes the client does — `fs/read_text_file` exists
  so the agent sees unsaved editor buffers, and `terminal/*` so commands run in the
  editor's terminal. Our `read`/`write`/`edit` go through `Host` with a `Policy`, and
  `bash`/`run_script` go through the sandbox backend (D5, D14). We must **not**
  advertise or use those client capabilities; the consequence is that an editor client
  sees the file on disk, not its dirty buffer.
- **Permission.** `session/request_permission` is baseline in ACP and forbidden by D17.
- **Sub-agents and profiles.** ACP has modes and commands, but nothing for D7 profile
  resolution, D6 capability bundles, or D12 checkpoints. All of it is ours.
- **Version churn.** ACP moved from v1 to v2 within the year, and v2's transports page
  still carries a draft. Adopting means tracking a moving spec through an SDK.

## Options considered

1. **Adopt ACP** (proposed). The supervisor and the kernel both speak ACP; grist-only
   traffic rides a documented `_grist/*` extension namespace (custom methods and
   `_meta` fields) that stock clients ignore. Transport is our unix socket, with a
   forwarder binary for clients that only know how to spawn a subprocess.
2. **Extend ACP.** Same, but the extensions are treated as first-class protocol
   changes: a forked schema, our own SDK build, and a divergence to maintain. Buys
   cleaner names for suspend/resume/checkpoint; costs compatibility with the very
   clients that motivated ACP.
3. **Own JSON-RPC schema + ACP shim.** One method per `Event` kind, mechanically
   derived from `event-schema.md`, plus a shim process translating our schema to ACP
   for third-party clients. Buys an exact 1:1 with the log and total freedom on
   suspend/resume/task semantics; costs a bespoke client in P1.9 (the Tauri shell, on
   three platforms) and a second protocol to keep in sync forever.

## Decision

**Option 1, with three qualifications.**

1. **Target ACP v2 and its Rust SDK** (`agent-client-protocol` 1.0.0) on both legs: the
   client ↔ supervisor leg over the D11 unix socket, and the supervisor ↔ kernel leg
   over stdio, where each kernel process serves exactly one session (D4). The kernel
   leg uses the same message set with a single `sessionId`, so one implementation
   serves both and the supervisor is a router, not a translator.
2. **Grist-only traffic lives in a `_grist/*` extension namespace**, specified in a new
   `docs/specs/protocol.md` alongside the event schema. It carries: the log-facing
   event kinds as a single `_grist/event` notification (the redacted `Event`, verbatim,
   so the projection is trivially faithful and provenance readers need nothing else);
   `_grist/cancel{scope}` for `Tool` and `Task` cancellation; and suspend/resume/
   checkpoint status. A stock ACP client that ignores all of it still gets a complete
   chat experience; our own client subscribes and gets the log.
3. **P1.9 ships no bespoke UI.** The first client is an off-the-shelf ACP client (Zed
   or a JetBrains IDE, both official, both on all three platforms) plus the forwarder
   binary and a documented configuration. The Tauri shell in the dev plan §13 moves to
   the phase that actually needs a purpose-built view (P3's fleet view), and is judged
   then against the same protocol.

Reasoning. The decisive term is the client requirement: "Windows, macOS and Linux, no
CLI" is otherwise a three-platform GUI project inside a phase whose purpose is a
protocol server. ACP turns that into a config file. The decisive risk is the opposite
one — that ACP's vocabulary silently becomes the system's model of itself. The
`_grist/event` notification is what prevents that: the log stays the source of truth
and the ACP surface stays a projection of it, which is what "every protocol message
maps to or from an `Event`" was asking for. Option 3 buys that property natively but
pays for it with the client work ACP exists to avoid, and Option 2 gives up the
compatibility that is the entire reason to be near ACP.

## Consequences

**Easier.** A working client on day one, on three platforms, with no UI code. The
kernel's stdio leg is ACP's native shape, so `KernelConfig.delta_sink` and
`KernelHandle::subscribe` connect almost directly to `session/update`. Third-party
agents can also be driven by our supervisor later, and our kernel can be driven by
other people's clients — useful for evals and for the P3 orchestrator.

**Harder.** We track a spec that is still moving, through an SDK, and must re-check the
v2 schema at every bump. Anything a stock client cannot express (per-tool cancel, task
suspension, checkpoint navigation) is second-class until our own client exists. Editor
clients will show file state from disk, not from their buffers, and users will notice.
An ACP client may expect `session/request_permission`; we will never send it, and the
UX consequence ("the agent just did it") must be documented rather than patched.

**Constrains later phases.** P3's remote and fleet work inherits ACP framing, which
means the websocket question (D11, backlogged) becomes "adopt ACP's streamable HTTP
transport when it stabilizes, or define ours" rather than a free choice. P2's
sub-agents (D6) and P4's harness edits have no ACP vocabulary and will keep growing the
`_grist/*` namespace; if that namespace ever outgrows the ACP core, this ADR should be
superseded by Option 3 rather than stretched.

## What P1.9 implements, starting from the launcher shape

`crates/orchestrator/tests/launcher.rs` already holds the reference path and is the
starting point: resolve the catalog entry against the compiled tool registry, turn
`profiles::KernelInputs` into `KernelConfig` + `SessionInit`, create the kernel, run.
The daemon wraps exactly that:

```text
session/new{cwd, …}      -> resolve profile (P1.8) -> KernelInputs
                         -> kernel_config_from(): tools filtered by the profile,
                            backend from `sandbox_backend` (refuse `none` outside dev
                            builds, D14), endpoint via host::endpoint_url_from_env,
                            provider from `Quirks` (P1.5), tool descriptions from the
                            model profile
                         -> Kernel::create(config, SessionInit) -> sessionId
session/prompt           -> KernelHandle::enqueue_user_message + Kernel::run
  session/update         <- KernelConfig.delta_sink (ModelDelta) for chunks
  _grist/event           <- KernelHandle::subscribe (Arc<Event>, post-redaction)
  elicitation/create     <- the `ask_user` tool through Host::ask_user (D17)
  stopReason             <- RunStop / TurnOutcome
session/cancel           -> KernelHandle::cancel(CancelScope::Turn)   (D15)
_grist/cancel{scope}     -> KernelHandle::cancel(Tool | Task)
session/resume           -> Kernel::open(config, ResumeCause) ; replayFrom -> the log
session/close            -> Kernel::end
```

The supervisor owns: the unix socket and its permissions, the peer-credential check,
the session-id → process map, process spawn and reaping, and the decision from
`Suspension.in_process_wakers` about whether a suspended kernel process stays alive
(`kernel-interface.md` §3.15).

## Resolutions of the four open questions

These were left open in the draft and are decided here.

### 1. Target ACP **v2**, with a v1 handshake only if the SDK makes it cheap

Version support is a hard compatibility gate, not a soft one: the client sends the
latest version it supports, an agent that cannot serve it **MUST** answer with the
latest *it* supports, and a client that gets back a different number **SHOULD** close
the connection and tell the user. There is no partial credit.

v2 is chosen because four of its additions are things we would otherwise have had to
invent in `_grist/*`: `session/resume` with `replayFrom` is our log replay; required
message ids let a message be replaced, which is exactly what the redactor's
`late_redaction` case needs; the idle state update is our `Idle`/`Suspended`
distinction; and `session/list` + `session/close` are what the supervisor's id → process
map needs anyway.

P1.9 starts by checking what Zed and the JetBrains plugin actually negotiate. If they
still speak v1 and `agent-client-protocol` does not make serving both versions cheap,
ship v1 first and treat v2 as a follow-on — the decision above is about the *model* we
build to, and the `_grist/*` namespace is unaffected by the version either way.

### 2. The blessed client is **Zed**, with a JetBrains IDE as the second known-good

Zed is official, runs on Windows, macOS and Linux (D14), is the protocol's reference
implementation, and accepts a custom agent as a settings entry:

```json
{ "agent_servers": { "grist": { "type": "custom", "command": "grist-connect", "args": ["--socket", "~/.grist/daemon.sock"], "env": {} } } }
```

`grist-connect` is the stdio↔socket forwarder from the decision above, so the blessed
client needs no code of ours at all. **The P1.9 exit demonstration is:** that snippet
checked into `docs/`, a session started from Zed against the supervisor's socket, a
tool call executed under the sandbox backend, an `ask_user` round trip, a cancel, and a
resume after suspend — all of it visible in the session log.

### 3. The Tauri shell is **deferred to P3**

P1.9's deliverable is the protocol server; the dev plan already allowed "a thin Tauri UI
*or* an ACP-compatible editor" (§13), and the app's second purpose — the browser UI for
the remote daemon (§11) — depends on the websocket transport that D11 backlogs pending
the security team. Building it in P1.9 means building it against a transport we do not
have. Consequence, recorded openly: until our own client exists, everything only it can
express (per-tool cancel, task navigation, checkpoint browsing) lives in `_grist/*` with
no UI in front of it. If that becomes painful before P3, the answer is a small ACP
client of our own, not a second protocol.

### 4. `_grist/event` carries the **whole redacted stream, opt-in, per session**

The events are post-redaction (D10) and are the same bytes the log already holds; any
client that reaches the socket has passed the peer-credential check and, running as the
same uid, can read the session's `.jsonl` file directly. A per-kind allowlist in the
protocol would therefore buy no confidentiality while creating a second, drifting
definition of "what a client may see" — and it would break the projection property that
motivated the notification in the first place.

So: a client receives nothing until it subscribes (`_grist/subscribe{sessionId, kinds?}`),
the subscription is scoped to one session, and `kinds` is a bandwidth filter, not a
security control. **The security boundary is the socket** — file permissions plus the
peer-credential check (D11) — and it stays there. If a genuinely lower-trust reader
appears later (a shared web UI over the backlogged websocket transport), it gets a
purpose-built filtered projection service, not a narrowed `_grist/event`.

## Sources

- ACP overview, initialization, prompt turn, session setup and transports:
  `https://agentclientprotocol.com/protocol/{overview,initialization,prompt-turn}`,
  `.../protocol/v2/{session-setup,transports,overview}`, retrieved 2026-09-06.
- Version negotiation rules: `https://agentclientprotocol.com/protocol/v2/initialization`.
- Client and agent registry: `https://agentclientprotocol.com/get-started/clients`.
- Zed custom agent configuration: `https://zed.dev/docs/ai/external-agents`.
- Repo inputs: `docs/design-decisions.md` (D4, D5, D9, D11, D14, D15, D17),
  `docs/specs/event-schema.md`, `docs/specs/kernel-interface.md` §3.13–§3.16,
  `crates/orchestrator/tests/launcher.rs`, `docs/agent-harness-dev-plan.md` §11, §13, §15.
