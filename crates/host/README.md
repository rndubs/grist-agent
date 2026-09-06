# `host`

**Responsibility:** `kernel::Host` implementations — filesystem under an `FsPolicy` (D5),
process spawn with an exact environment (D10), network under a `NetPolicy`, secrets as
handles with a `SecretResolver` that registers every value with the shared `Redactor`
(D10, §7.5), and `ask_user` through a pluggable prompter (D17). Filled in at P1.6
(`native`) and P3.3 (`remote-client`).

**Mutable by the evolve loop?** No.

## Shape

| Item | What it is |
|---|---|
| `host::native::NativeHost` | The in-process host. Implements `kernel::Host` **and** `kernel::SecretResolver`. `name() == "native"`. Built from an `Arc<Redactor>` and an `Arc<dyn SecretSource>`; `with_prompter` attaches a `UserPrompter`, `with_net_config` sets proxy/CA options. `Debug` prints secret names only. |
| `host::secrets::{SecretSource, EnvSecretSource, MapSecretSource}` | Where secret *names* are looked up. `EnvSecretSource::from_env()` snapshots the environment once at construction (never re-read); `from_snapshot(map)` for launchers that scrub; `MapSecretSource` is an in-memory map. |
| `host::prompt::{UserPrompter, NoUserPrompter, ChannelPrompter, PendingQuestion}` | `ask_user` transport. `NoUserPrompter` (default) returns `HostError::NoUser`. `ChannelPrompter::new(cap)` returns the prompter and an `mpsc::Receiver<PendingQuestion>`; the client calls `answer(text)` or `decline()`. |
| `host::ask_user::AskUserTool` | The `ask_user` tool (§7.10): `ToolKind::Stateless`, capabilities `[]`, input `{question, options?, allow_free_text? = true}`, output `{question_id, answer: string \| null, declined: bool}`, `question_id = "q{turn}-{tool_use_id}"`. |
| `host::remote_client::RemoteClientHost` | P3.3 stub; every method returns `HostError::Io("remote-client host is not implemented until P3")`. Its module docs describe the wire shape. `name() == "remote-client"`. |
| `host::endpoint_url_from_env(name)` | `GRIST_ENDPOINT_<NAME>_URL` (upper-cased, `-` → `_`) for the launcher (`profile-schema.md` §2.1). `endpoint_url_from_lookup` is the pure form; `endpoint_env_var` the mapping. |
| `host::host_allowed(allow, host, port)` | The `NetAllow` check used by `NetHandle::send`. |

## Policy enforcement rules

### Filesystem (D5) — `read_file`, `write_file`, `list_dir`, `stat`, `remove`

Every call takes the `FsPolicy` and, before touching the disk:

1. The path must be absolute, else `Denied(PathDenied)`.
2. It is normalized lexically: `.` is dropped; any `..` is `Denied(PathDenied)` even when it
   would land inside a mount.
3. The **lexical** path is checked with `FsPolicy::check` (`Ro` for read/list/stat, `Rw` for
   write/remove; overlapping mounts: the most specific path wins, so a nested `Ro` mount under
   an `Rw` mount denies writes).
4. Symlinks are resolved for the existing prefix: the deepest existing ancestor is
   canonicalized and the missing tail re-appended. A **dangling** symlink is followed by hand
   (so a write cannot land at its target unchecked). The **resolved** path is checked again
   against the policy with each mount path resolved the same way.

Both checks must pass. Consequences: a symlink inside a mount that points outside is denied
for every operation; a path outside every mount is denied even if it links into a mount
(bwrap would not show it at all); a link to an `Ro` file inside an `Rw` mount cannot be
written or removed (the target's mode decides). Operations run on the resolved path, except
`remove`, which acts on the lexical path so a link — not its target — is removed.

I/O errors map to `HostError::NotFound` (including `write_file` with a missing parent: no
directories are created) or `HostError::Io`. `Metadata.modified` is RFC 3339 UTC.

### Process — `spawn(policy, cmd)`

Unsandboxed by design (§3.9: for the sandbox crate's launchers and the in-process waker; tools
use `SandboxBackend`). The child environment is **exactly** `cmd.env` (`env_clear()` first;
nothing is inherited, D10). stdin/stdout/stderr are piped; `cmd.stdin` is written then
closed; `kill_on_drop` is set. If `policy.programs` is non-empty, `cmd.program` must match an
entry exactly, either as written or by basename, else `Denied(ProgramDenied)`.

`ChildProcess::wait` collects stdout/stderr and enforces `policy.timeout` (counted from
spawn): on expiry SIGTERM, 1 s grace, SIGKILL, and `timed_out: true`. `exit_code`, `signal`,
and `duration` are reported; a second `wait` returns the same output. `terminate(grace)`
sends SIGTERM (`nix::sys::signal::kill`), then SIGKILL after `grace` unless a concurrent
`wait` observed the exit; it does not reap — call `wait`. `pid()` is the OS pid.

### Network — `network(policy)`

`!policy.enabled` → `Denied(NetDenied("*"))` with no client built. Otherwise a `NetHandle`
over one `reqwest` client (rustls). `send` parses the URL and checks its host **before any
I/O**: allowed under `NetAllow::Any`, or when the lowercased host (trailing `.` stripped) is
in the set, or `host:port` is (`port` is the URL's explicit or scheme-default port), else
`Denied(NetDenied(host))`. The body is streamed as `Vec<u8>` chunks; failures are
`HostError::Net`.

Proxy: reqwest's system-proxy default (`HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY`), off with
`NetConfig::without_proxy()`. CA bundle: `NetConfig::from_env()` (what `NativeHost::new` uses)
reads `GRIST_CA_BUNDLE`, else `SSL_CERT_FILE`, and adds the PEM bundle as extra roots; an
unreadable bundle makes `network()` fail with `HostError::Net` rather than connecting without it.

### Secrets (D10, §7.5)

`secret(name)` returns `SecretHandle::with_locator(name, "<kind>:<name>")` iff the source knows
`name`, else `UnknownSecret`; never a value. `resolve_secret(handle)` (the `SecretResolver`
impl, handed to provider clients only) looks the name up, wraps it in `SecretString`, and
**registers it with the shared `Redactor` before returning** (as a named secret, so a
too-short value surfaces in the `secret_too_short` warning). A handle without a locator
(deserialized) resolves too.

### `ask_user` (D17)

`Host::ask_user` forwards to the configured `UserPrompter`. Permission is never asked;
sandbox policy already answered it.

## Tests

`tests/fs_policy.rs`, `tests/spawn.rs`, `tests/net.rs`, `tests/secrets.rs`,
`tests/ask_user.rs`, `tests/endpoint.rs` — one test per rule above. Tests cannot set
environment variables (`unsafe_code = forbid`), so the env-backed paths use variables cargo
already sets (`CARGO_PKG_NAME`, `CARGO_MANIFEST_DIR`) and the pure lookup variants.
