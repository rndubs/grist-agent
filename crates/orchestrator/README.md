# `orchestrator`

**Responsibility:** Placement, wakers, fleet, agent-to-agent messaging, trust tiers (P3). Seeded
in P1.9 by the launcher and the ACP protocol server (ADR-0004, D4, D11).

**Mutable by the evolve loop?** No.

See the crate table in `docs/agent-harness-dev-plan.md` §3.1 and the dependency DAG in
`crates/README.md` (`orchestrator → host, sandbox, profiles, providers`).

## What is here (P1.9)

```
src/launcher.rs        profiles::resolve → KernelConfig + SessionInit; create_session / resume_session;
                       SessionRecord (<state>/sessions/<id>.toml); backend selection (D14);
                       endpoint URL, provider from quirks, tool descriptions (Described), ask_user
src/acp/agent.rs       the ACP agent component: initialize, session/new|prompt|cancel|close|list|load|resume,
                       elicitation for ask_user (D17), the _grist/* handlers
src/acp/session.rs     the task that owns a Kernel and drives `run` (Driver / Cmd)
src/acp/project.rs     Event / ModelDelta → session/update (pure)
src/acp/grist.rs       _grist/subscribe|unsubscribe|event|cancel|status
src/acp/daemon.rs      the supervisor: unix socket (0600, peer uid check), one grist-kernel per session, typed forwarding
src/acp/connect.rs     stdio ↔ socket (or TCP) forwarder
src/bin/grist_kernel.rs   one session over stdio            (the process the daemon spawns; also usable alone)
src/bin/grist_daemon.rs   grist-daemon [--socket] [--kernel-bin]
src/bin/grist_connect.rs  grist-connect [--socket PATH | --tcp HOST:PORT]
```

The protocol is specified in `docs/specs/protocol.md`; the client setup in `docs/clients/zed.md`.

## Tests

`cargo test -p orchestrator --all-features` (the `dev-sandbox-none` feature lets the launcher pick
the `None` backend on a host without `bwrap`, D14):

- `tests/acp_agent.rs`: an SDK client against `agent_component` in process — a coding turn with
  tool-call and text updates, `_grist/status` and `session/list`; `ask_user` through
  `elicitation/create` with the event subscription seeing `ask_user`/`user_answer`; subscription
  kind filters; `session/cancel` → `stopReason: cancelled`; `run_script` suspend → waker → resume
  inside one prompt; `session/load` replaying history and continuing; single-session refusal.
- `tests/daemon.rs`: the daemon (in-process `serve`) spawning the real `grist-kernel` binary per
  session, a raw socket client and the real `grist-connect` binary over its stdio, against a fake
  OpenAI-compatible SSE endpoint through the real `providers` client; socket mode `0600`.
- `tests/launcher.rs`, `tests/exit_criteria.rs`: the reference launcher shape and the Phase 1 exit
  criteria sessions (unchanged from P1.8).

Not testable here: the peer-uid rejection (every test runs as one uid) and a real editor session
(`docs/clients/zed.md` §4).
