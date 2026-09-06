# Zed as the first client (P1.9, ADR-0004)

Zed is an official ACP client on macOS, Linux and Windows, so the first grist client is a
settings entry, not an application. Everything below also works for any other ACP client that
can spawn a subprocess (JetBrains via its ACP plugin is the second known-good target).

## 1. Local, single session, no daemon

Zed spawns `grist-kernel` directly; it serves exactly one session over stdio (D4).

```json
{
  "agent_servers": {
    "grist": {
      "type": "custom",
      "command": "/path/to/grist-kernel",
      "args": [],
      "env": {
        "GRIST_PROFILES_DIR": "/path/to/grist-agent/profiles",
        "GRIST_ENDPOINT_LITELLM_CI_URL": "http://127.0.0.1:4000/v1",
        "LITELLM_CI_API_KEY": "sk-…"
      }
    }
  }
}
```

The checked-in copy is `docs/clients/zed-agent-servers.json`. `GRIST_STATE_DIR` defaults to
`~/.grist`; the endpoint variable is named after the model profile's `endpoint`
(`profile-schema.md` §2.1, `GRIST_ENDPOINT_<NAME>_URL`), and the bearer secret after its
`quirks.auth` (`bearer:<NAME>`). A development build may add `"GRIST_SANDBOX_BACKEND": "none"`
on a host without `bwrap` (D14; the kernel logs a `warning` for it in every such session).

## 2. Through the daemon (many sessions, local or remote)

Start the supervisor once (it owns `~/.grist/daemon.sock`, mode 0600, and checks the peer's
uid, D11):

```sh
grist-daemon                       # or: grist-daemon --socket /some/path.sock
```

and let Zed spawn the forwarder:

```json
{ "agent_servers": { "grist": { "type": "custom", "command": "/path/to/grist-connect", "args": [], "env": {} } } }
```

`grist-connect` connects to `~/.grist/daemon.sock` (`--socket PATH` to change it) and forwards
Zed's stdio unchanged. For a daemon on the HPC login node, forward its socket with ssh first
(D11: no websocket in this phase):

```sh
ssh -N -L /tmp/grist.sock:/home/me/.grist/daemon.sock login-node   # macOS / Linux
# then: "args": ["--socket", "/tmp/grist.sock"]
ssh -N -L 7411:/home/me/.grist/daemon.sock login-node               # Windows (no unix sockets)
# then: "args": ["--tcp", "127.0.0.1:7411"]
```

## 3. What Zed shows, and what it cannot

- Every model message, thought, tool call and tool result of a turn (`docs/specs/protocol.md`
  §4). File contents come from disk, not from Zed's unsaved buffers (ADR-0004).
- `ask_user` questions arrive as elicitations (a form with one `answer` field, a choice list
  when the tool gave options and no free text). Zed never gets a permission prompt: sandbox
  policy already decided (D17).
- **Cancel** is Zed's stop button (`session/cancel`, turn scope). Per-tool and per-task cancel,
  the event stream, and the session's log-level status exist only in `_grist/*` and have no UI
  until our own client (P3).
- A long-running `run_script` looks like a pause: the kernel suspends, the process-exit waker
  resumes it, and the turn continues (visible in the session log as `suspended` → `task_update`).
- Reopening a session is `session/load` (Zed's session history), which replays the log's user
  and agent messages and continues from the last checkpoint.

## 4. The P1.9 demonstration

ADR-0004 names the exit demonstration: the snippet above, a session started from Zed against
the supervisor's socket, a tool call under the sandbox backend, an `ask_user` round trip, a
cancel, and a resume after suspend, all visible in the session log. Every step except "from Zed"
is asserted in `crates/orchestrator/tests/acp_agent.rs` and `tests/daemon.rs` (an SDK client in
place of Zed, the real `grist-kernel` and `grist-connect` binaries). The Zed run itself needs a
human at an editor (🧑) and is recorded in the plan when it happens.

To watch the raw traffic in Zed: `dev: open acp logs`.
