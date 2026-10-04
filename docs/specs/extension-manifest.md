# Extension API and out-of-process manifest

- **Version:** 0.1 (2026-10-04), lands with P2.1
- **Decisions:** D5, D6, D7, D8, D10, D15; ADR-0001
- **Code:** `crates/ext` (`compiled`, `manifest`, `loader`, `process`); launcher wiring in
  `crates/orchestrator/src/launcher.rs`; example `examples/text-stats/`

The harness has two extension tiers (D8, ADR-0001). This spec fixes the API of each and the
admission rule between an out-of-process extension and the profile that loads it.

## 1. Compiled tier

Rust middleware and first-party tools. Compiled into the build; never authored by the agent.

```rust
pub struct SessionSetup { pub workdir: PathBuf, pub sandbox: Arc<dyn SandboxBackend> }

pub trait Extension: Send + Sync {
    fn name(&self) -> &str;
    fn tools(&self, setup: &SessionSetup) -> Vec<Arc<dyn Tool>> { vec![] }
    fn middleware_names(&self) -> Vec<String> { vec![] }
    fn middleware(&self, name: &str, config: &Value) -> Result<Arc<dyn Middleware>, ExtError>;
}

pub struct ExtensionSet { /* … */ }
impl ExtensionSet {
    pub fn add(&mut self, ext: Arc<dyn Extension>) -> Result<(), ExtError>;
    pub fn tools(&self, setup: &SessionSetup) -> Result<Vec<Arc<dyn Tool>>, ExtError>;
    pub fn tool_decls(&self, workdir: &Path) -> Result<Vec<(String, ToolKind, Vec<Capability>)>, ExtError>;
    pub fn middleware_names(&self) -> BTreeSet<String>;
    pub fn middleware_entry(&self, name: &str, priority: i32, source: MiddlewareSource,
                            config: &Value, config_hash: Option<Hash>) -> Result<MiddlewareEntry, ExtError>;
}
```

- An `ExtensionSet` is the one place the launcher gets compiled tools, the validator's
  `Registry.tools` / `Registry.middleware` (`profile-schema.md` §7.1), and middleware instances.
- Rejected when added: a duplicate extension name (`DuplicateExtension`), a middleware name
  another extension already provides (`DuplicateMiddleware`).
- Rejected when tools are built: a name failing `is_valid_tool_name` (`InvalidToolName`), a
  duplicate across extensions (`DuplicateTool`), and a name starting with `mcp.` or `ext.`
  (`ReservedPrefix`; `profile-schema.md` §11.5).
- `tool_decls` constructs the tools against a backend that refuses every launch, so declarations
  need no real sandbox.
- Shipped compiled extensions: `ext::BaseTools` (`read`, `write`, `edit`, `bash`, `run_script`,
  `python`; the implementations stay in `sandbox::tools`) and the launcher's `ask_user` (D17).

## 2. Out-of-process manifest

An out-of-process extension is a directory holding `extension.toml`. Authoring one at runtime is
writing that directory (ADR-0001): no compile step, no restart beyond the next session start.

```toml
schema_version = 1                       # required; only 1 is read

[extension]
name = "text_stats"                      # required; [a-z][a-z0-9_-]*
version = "0.1.0"                        # required; opaque
description = "…"                        # optional
kind = "stateless"                       # required; "stateless" | "session" (D5)
runtime = "process"                      # optional; "process" only. "wasm" is reserved (ADR-0001)
command = ["python3", "${ext}/server.py"] # required; element 0 is the program
cwd = "${workdir}"                       # optional; default ${workdir}
capabilities = ["fs.ro:${workdir}", "proc:python3"]

[[tools]]                                # at least one
name = "count"                           # required; [a-z][a-z0-9_-]*
description = "…"                        # required; what the model sees
input_schema = { type = "object", … }    # exactly one of input_schema / schema_file
# schema_file = "schemas/count.json"     # relative to the extension directory
```

Rules:

| Rule | Error |
|---|---|
| Every table is closed: an unknown key anywhere is rejected | `Invalid` naming the dotted key |
| `schema_version` must be `1` | `Invalid` |
| Placeholders `${ext}` (the extension directory, canonical), `${workdir}`, `${home}` are allowed only as the leading characters of a `command` element, `cwd`, or an atom operand; any other `${` is rejected | `Invalid` |
| `cwd` must be absolute without `..` after expansion | `Invalid` |
| `capabilities` may contain only `fs.ro`, `fs.rw`, `net` and `proc` atoms (`profile-schema.md` §11). `tool:`/`spawn:` are derived elsewhere and `secret:` never reaches a sandbox (D10) | `Invalid` |
| `fs.ro:<extension directory>` is added implicitly to `capabilities`: the process may read its own files, and the profile must grant that like any other atom. Found under bwrap: nothing outside the policy's mounts is guaranteed visible, and the scratch tmpfs hides everything under `/tmp`. An extension under `${workdir}/.grist/extensions/` is covered by the usual `fs.rw:${workdir}`, and is read-only inside its own sandbox because the more specific mount wins | — |
| `capabilities` must contain `proc:<command[0]>`, so the launcher's program check always has a list to enforce (an empty program list would allow anything on `PATH`) | `Invalid` |
| Tool names unique; `ext.<extension>.<tool>` must pass `is_valid_tool_name` (≤ 64 chars) | `Invalid` |
| A schema is a JSON object with `"type": "object"`; a `schema_file` must be relative, contain no `..`, and resolve (through symlinks) inside the extension directory | `Invalid`, `Io`, `Parse` |
| The manifest hash is `b3` of the file bytes (`Manifest.hash`) | — |

Every tool of an extension declares the extension's capabilities, so they share one derived
policy. Each is registered as `ext.<extension>.<tool>`.

Not in v1 (additive later, since tables are closed): a `[dependencies]` table materialised into
a per-extension venv at install time (ADR-0001), and `runtime = "wasm"`.

## 3. Wire protocol

The stdio subset of MCP: newline-delimited JSON-RPC 2.0, one object per line, on the process's
stdin/stdout. ADR-0001 left "MCP verbatim or a subset" open; this is a **strict subset**, so the
same launcher serves agent-authored tools and the P2.2 MCP client.

### 3.1 Methods

| Method | Who sends it | Params | Result |
|---|---|---|---|
| `tools/call` | the tool on every invoke | `{name, arguments}`; `name` is the extension-local tool name | `CallToolResult` (§3.3) |
| `initialize` | only `ext::probe` | `{protocolVersion, capabilities: {}, clientInfo}` | MCP `InitializeResult`; `serverInfo` is kept |
| `tools/list` | only `ext::probe` | `{}` | `{tools: [{name, …}]}`; every manifest tool must be listed |

**Relaxed handshake.** A guest MUST answer `tools/call` without a prior `initialize`. The
schemas the model sees come from the manifest, so no handshake is needed to expose a tool, and a
`Session` process relaunched by the kernel after a crash or cancel (`kernel-interface.md` §7.9)
can be called immediately.

### 3.2 Process lifetime per kind (D5)

- `stateless`: `SandboxBackend::launch_stateless` under the tool's derived policy, with the
  request line (`id: 1`) on stdin. The guest answers and exits at end of stdin. The first stdout
  line with `id: 1` (or with an `error`) is the response. No response means a `ToolError::Failed`
  carrying the exit status and the stderr tail.
- `session`: the kernel launches `session_command()` lazily under the derived policy, one
  process per tool per session (§7.9). `invoke` sends `tools/call` through
  `ToolContext::session_process()` (`JsonRpcSession`).

Both kinds inherit the launchers' guarantees: scrubbed environment (D10), the policy timeout, and
SIGTERM, then SIGKILL after the grace period, on cancel or timeout (D15). A guest SHOULD exit
promptly on SIGTERM, because under `--unshare-pid` it is PID 1 and ignores an unhandled one.

### 3.3 Results

| Response | `invoke` returns | The model sees |
|---|---|---|
| JSON-RPC `error` | `ToolError::Failed("extension `x` protocol error …")` | an error result |
| `result.isError == true` | `ToolError::Failed(<text content joined>)` | an error result with the tool's own message. A failure in user code is a successful call (ADR-0001) |
| `result.structuredContent` present | `ToolResult::Value(structuredContent)` | JSON |
| otherwise | `ToolResult::Blocks` of the `text` items | text |
| a non-`text` content item, a non-object result, or no content at all | `ToolError::Failed("… malformed …")` | an error result |
| sandbox timeout / cancel / program refused | `ToolError::Timeout` / `Cancelled` / `Denied` | per the kernel's normal handling |

Oversized results are spilled by the kernel (D12) like any other tool's.

## 4. Admission

A profile names extension directories in `[extensions].paths` (`profile-schema.md` §3.12). At
session start, both create and resume, the launcher calls
`ext::load_all(paths, placeholders, grants_resolved, sandbox_limits)`:

1. Each directory's manifest is loaded and validated (§2).
2. **Grant check.** `kernel::derive_policy_with(manifest.capabilities, grants_resolved, limits)`
   must succeed: every required atom must be `narrower_than` some granted atom
   (`profile-schema.md` §11.4). Otherwise the extension is refused as a whole with
   `ExceedsGrants { extension, cap }` naming the first uncovered atom, and **the launch fails**
   (`LaunchError::Extension`). A partially admitted extension is never exposed.
3. Two directories declaring the same extension name: `DuplicateManifest`.
4. Admitted tools are added to `KernelConfig.tools`, and their derived `tool:ext.<x>.<y>` atoms
   are added to `KernelConfig.grants` (`profile-schema.md` §3.3). They are therefore visible in
   `session_created.tools` and `.grants`.

The kernel then repeats the same derivation for every tool at construction (§7.7), so a tool that
somehow bypassed admission still cannot be registered. The check is a single code path,
`derive_policy_with`, in both places.

Because `fs.rw:${workdir}` covers `${workdir}/.grist/extensions/…`, an agent can write a new
extension and a project override (layer 3) that lists it. That is the intended way for the agent
to author a tool. The override can only narrow grants (§4.2), so the tool can never receive more
than the agent profile granted.

`ext::probe(manifest, backend, grants, limits)` is optional verification: it launches the process
as a session under the derived policy (refusing first if the grants do not cover it), sends
`initialize` and `tools/list`, terminates it, and fails with `Probe` if a manifest tool is not
listed. The launcher does not probe at session start.

## 5. Open questions for the reviewer

1. Extension loads are not yet logged as events. `ProfileKind` has no `extension` variant, and
   adding one is a kernel change that needs an ADR. Until then the manifest hash
   (`Manifest.hash`) is computed but not recorded, and the evidence of a load is
   `session_created.tools` / `.grants`.
2. A refused extension fails the launch rather than being skipped with `rejected = true`, which
   is what skills do (`profile-schema.md` §3.5). Failing closed is the safer default for
   executable code; revisit if project overrides that narrow grants turn out to break sessions
   too often.
3. Dependencies (`uv` venv per extension, ADR-0001) are not implemented; extensions must use
   what the sandbox image provides.
