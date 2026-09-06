# `sandbox`

**Responsibility:** turn a `kernel::SandboxPolicy` into enforcement (bwrap arguments, scrubbed
environment, program check, timeout/cancel), provide the `SandboxBackend` implementations and the
JSON-RPC session launcher, and ship the six base tools (P1.7).

**Mutable by the evolve loop?** Policy yes (the grants a policy is derived from); enforcement no.

Specs: `docs/specs/kernel-interface.md` §3.6, §3.9, §3.12, §7.1, §7.5, §7.9;
`docs/specs/profile-schema.md` §3.11; ADR-0001; D1, D5, D6, D10, D14, D15.

## Shape

```
sandbox
├── env        scrubbed_env / scrubbed_env_with (D10), check_program (argv[0] check)
├── bwrap      policy_to_args (pure), BwrapBackend (name "bwrap")
├── launch     supervise: timeout → Timeout, cancel → SIGTERM/grace/SIGKILL → Cancelled
├── session    JsonRpcSession (SessionProcess): newline-delimited JSON-RPC 2.0 over stdio
├── none       NoneBackend (name "none") — feature `dev-sandbox-none` only
└── tools      read, write, edit, bash, run_script, python; base_tools(), tool_decls()
```

Policy derivation is `kernel::derive_policy_with`, re-exported here as `sandbox::derive_policy`
/ `derive_policy_with`; this crate never derives anything itself.

## `bwrap` argument mapping (`policy_to_args`)

The inner shape of `spikes/sandbox-nesting/inner-tool-call.sh` (ADR-0002 pending):

| Policy / command field | Arguments |
|---|---|
| always | `--unshare-user --unshare-pid --unshare-ipc --unshare-uts` |
| `net.enabled == false` | `--unshare-net` |
| `net.enabled == true` | `--share-net` (the host **allowlist** is not enforceable by bwrap in P1) |
| always | `--uid 1000 --gid 1000 --ro-bind / /` (read-only base image) |
| `scratch_tmpfs_mb` | `--size <mb × 1048576> --tmpfs /tmp` (`--size` precedes the `--tmpfs` it sizes; the tmpfs precedes the policy mounts so a mount under `/tmp` is not shadowed by it) |
| each `mounts[i]`, shallowest first | `--bind p p` (`Rw`) / `--ro-bind p p` (`Ro`); the most specific path is last and wins |
| always | `--proc /proc --dev /dev` |
| `env_allowlist` ∩ process env, then `cmd.env`, then `HOME=/tmp` | `--clearenv` followed by one `--setenv NAME VALUE` per entry |
| `cmd.cwd` | `--chdir <cwd>`, default `--chdir /tmp` |
| always | `--die-with-parent --new-session -- <program> <args…>` |
| `timeout` | not an argument: the launcher enforces it (`SandboxError::Timeout`) |
| `programs` | not an argument: the launcher checks `cmd.program` (name or basename) before spawning (`SandboxError::Launch("program not permitted …")`) |

Environment rules (`scrubbed_env`): only names in `env_allowlist` that exist in the kernel
process environment are copied; a name matching `SECRET_LIKE_ENV` is refused outright (the
derivation already rejects it; the launcher is the last line of defense); the tool's own explicit
`Command::env` is added; `HOME=/tmp` is forced last. Nothing else is inherited.

Launch paths:

- **Stateless:** `bwrap` is exec'd through `Host::spawn` (the one non-kernel use §3.9 permits)
  under `ProcPolicy{programs: {bwrap}, env_allowlist, timeout}`; stdin is fed by the host; the
  supervisor applies the timeout and the cancellation token (SIGTERM, grace of 5 s by default,
  SIGKILL). A dropped launch future terminates the child in the background.
- **Session:** `bwrap` is spawned with `tokio::process::Command` directly, with identical
  arguments and environment (`env_clear()` + the scrubbed map, `kill_on_drop`). This is a
  deliberate deviation: the RPC transport needs piped stdin/stdout and `ChildProcess` exposes no
  pipes.

## JSON-RPC session protocol (`JsonRpcSession`)

One JSON object per line, ids assigned by the session, one call in flight at a time:

```
→ {"jsonrpc":"2.0","id":1,"method":"eval","params":{"code":"x = 41"}}
← {"jsonrpc":"2.0","id":1,"result":{"ok":true,"stdout":"","stderr":"","value":null,"error":null}}
→ {"jsonrpc":"2.0","id":2,"method":"nope","params":{}}
← {"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"unknown method 'nope'"}}
```

- `result` → `RpcResponse::Result(value)`; `error` → `RpcResponse::Error{code, message, data}`.
  JSON-RPC errors are **protocol** failures (unknown method, bad params, parse error). A failure
  in user code is a successful call whose result says so (`ok:false` + structured `error`).
- Per call: the policy timeout and the caller's token. On either firing: SIGTERM → grace →
  SIGKILL (mandatory: PID 1 under `--unshare-pid` ignores an unhandled SIGTERM), the session is
  marked dead, the call returns `Timeout` / `Cancelled`. `is_alive()` is then false and the kernel
  relaunches once at the next invoke (§7.9).
- EOF from the child → `SandboxError::Exited` (or `Rpc` carrying the stderr tail when there is
  one); malformed lines and id mismatches → `SandboxError::Rpc`.
- stderr is drained into a 64 KiB tail (`stderr_tail()`); `terminate()` is idempotent.

## The `None` backend rule (D14)

`NoneBackend` exists only under the `dev-sandbox-none` feature. It applies the same scrubbed
environment, program check, timeout and cancel handling, and the same session transport, but no
mounts, namespaces, tmpfs or network isolation. `name() == "none"`, which the kernel logs as
`warning{class: "sandbox_backend_none"}` at every session start and resume. A release build MUST
NOT contain it; CI asserts it with `examples/none_backend_symbol.rs`:

```
! cargo check -p sandbox --example none_backend_symbol                                # must fail
  cargo check -p sandbox --example none_backend_symbol --features dev-sandbox-none    # must pass
```

## Base tools (`sandbox::tools`)

| Tool | Kind | Declares | Input | Returns |
|---|---|---|---|---|
| `read` | Stateless, in-process (`Host::read_file` under `policy.fs()`) | `fs.ro:<workdir>` | `{path, offset?, limit?}` | `{content (numbered lines), lines, truncated}` |
| `write` | Stateless, in-process | `fs.rw:<workdir>` | `{path, content}` | `{bytes}` |
| `edit` | Stateless, in-process | `fs.rw:<workdir>` | `{path, old_string, new_string, replace_all?}` — `old_string` must occur exactly once unless `replace_all` | `{replacements}` |
| `bash` | Stateless, sandboxed (`bash -c`) | `fs.rw:<workdir>`, `proc:bash` | `{command, cwd?, timeout_secs?}` (`timeout_secs` may only lower the policy timeout) | `{exit_code, signal, stdout, stderr, timed_out, duration_ms}` |
| `run_script` | Stateless, sandboxed, returns `Task` (D1) | `fs.rw:<workdir>`, `proc:bash` | `{path, args?, cwd?}` | `TaskHandle{id: t<turn>-<tool_use_id>, Running, description: "<path> <args>"}`; the outcome (same shape as `bash`, `is_error` iff non-zero exit / timeout / kill) arrives through the in-process waker |
| `python` | Session, sandboxed (`python3 -c <repl_server.py>`) | `fs.rw:<workdir>`, `proc:python3` | `{code}` | the REPL's `{ok, stdout, stderr, value, error{type, message, traceback}}` — `ok:false` is still `Ok(Value)` |

`base_tools(workdir, sandbox)` returns all six; `tool_decls(workdir)` returns
`(name, kind, capabilities)` for the profiles validator. `run_script` owns an
`Arc<dyn SandboxBackend>` because its completion future must be `'static` while `ToolContext`
only lends the backend for the invocation; its task is bound to the registered future (Task
scope), not to the invocation token, so a cancelled turn leaves it running (§7.1).

## Tests

`cargo test -p sandbox --all-features`. Everything runs on a plain Linux host with `bash` and
`python3`; the tests in `tests/bwrap.rs` skip (printing `skipping: bwrap not available`) when
`bwrap` is not on `PATH`. They were first run for real on 2026-09-06 in a Debian bookworm
container (bwrap 0.8.0, Python 3.11) under a rootless Podman machine (kernel 7.1.8); that run
found and fixed the tmpfs/mount ordering above. A container is a legitimate host for these
assertions, but it is not the HPC login node: ADR-0002 still waits on the P0.1 run there.

Running the whole workspace on macOS trips three host assumptions in test harnesses
(`host`'s spawn test hardcodes `/usr/bin/cat`; a `sandbox` test compares `/tmp` with the
resolved `/private/tmp`; the tests' `process_gone` reads `/proc/<pid>/status`). They are not
product bugs; run the suite on Linux, e.g. in a container with the repo bind-mounted.
