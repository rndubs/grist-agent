# Spike P0.3 — Out-of-process tool mechanism

- **Milestone:** P0.3 (feeds ADR-0001 in P0.4)
- **Date:** 2026-09-06
- **Code:** `spikes/ext-mechanism/` (throwaway; `run_all.sh` reproduces every number here)
- **Decision it feeds:** `docs/adr/0001-out-of-process-tool-mechanism.md`

## Question

D8 narrowed the extension question: middleware and first-party tools are compiled
Rust behind `ext`; only *agent-authorable* extensions are out of process. This
spike picks the mechanism for those. The test case is the one the dev plan (§7)
calls the primary domain tool: a persistent Python REPL as a `Session`-kind tool
(D5) — stateful, long-lived, must run inside the inner sandbox, every call an RPC
into it.

Two prototypes, same REPL contract (`eval(code) -> {ok, stdout, stderr, value,
error{type, message, traceback}}`, `reset()`, `info()`):

- **A — process + JSON-RPC.** A Rust (tokio) host spawns a Python process and
  speaks newline-delimited JSON-RPC 2.0 over its stdio.
- **B — WASM Component Model.** The same REPL compiled to a component by
  componentize-py and hosted in wasmtime from Rust.

## Environment and versions

| Item | Version |
|---|---|
| Host | Linux 6.18 container, 4 vCPU, 15 GB RAM, x86_64. No `bwrap`, no Docker daemon. |
| rustc / cargo | 1.94.1 (repo pin) |
| tokio / serde_json / libc | 1.53.1 / 1.0.151 / 0.2.189 |
| wasmtime / wasmtime-wasi crates | 47.0.4 / 47.0.4 (cranelift-codegen 0.134.4) |
| componentize-py | 0.25.0 from PyPI; embeds **CPython 3.14.0 (wasi-sdk, Clang 22.1.0)** |
| Host Python for A | 3.11.15 (system) and a `uv` venv with numpy 2.4.6 |
| uv | 0.8.17 |
| Network | crates.io and PyPI reachable through the proxy. **github.com and huggingface.co return 403 at CONNECT** (egress policy). |

## Prototype A: process + JSON-RPC over stdio

Files: `spikes/ext-mechanism/proto-a/repl_server.py` (111 lines, the tool),
`proto-a/src/main.rs` (284 lines, host + measurements).

Protocol: one JSON object per line. Requests `{"jsonrpc":"2.0","id":n,"method":
"eval","params":{"code":...}}`. Exceptions raised by *user code* are not JSON-RPC
errors — the call succeeded — they come back as `ok:false` with a structured
`error` record. JSON-RPC errors (`-32601` etc.) are reserved for protocol
failures. The host spawns with `env_clear()` (D10) and `kill_on_drop`.

Cancellation (D15): a per-call deadline; on expiry the host sends `SIGTERM` to
the child, waits up to 1 s, escalates to `SIGKILL`, and reports the signal, exit
status and kill latency. The session is then respawned and the test asserts the
namespace is fresh.

```
cd spikes/ext-mechanism/proto-a
cargo build --release
./target/release/proto-a-jsonrpc --out ../results/proto-a-system-python.json
uv venv -p 3.11 .venv && uv pip install -p .venv/bin/python numpy
./target/release/proto-a-jsonrpc --python .venv/bin/python --out ../results/proto-a-venv-numpy.json
```

Everything asked for worked on the first run: state persisted (`x = 41` in call 1,
`x + 1 -> "42"` in call 2), `1/0` came back as `{ok:false, error.type:
"ZeroDivisionError"}`, an unknown method came back as JSON-RPC `-32601`, an
infinite loop was killed by SIGTERM about 1 ms after the 500 ms deadline, the
restart took ~22 ms, and in the venv `import numpy` plus a 1e6-element sum took
67 ms on first use and 25 ms on reuse (the array `a` persisted between calls).

## Prototype B: WASM Component Model

Files: `spikes/ext-mechanism/proto-b/wit/repl.wit` (23 lines),
`proto-b/guest-py/app.py` (54 lines, the tool), `proto-b/host/src/main.rs`
(259 lines, wasmtime host + measurements), `proto-b/build-guest.sh`.

Guest routes, tried in the order the task specified:

| Route | Outcome |
|---|---|
| (a) componentize-py producing a Python component | **Worked.** `componentize-py==0.25.0` installs from PyPI and bundles its own CPython 3.14.0/WASI runtime, so no other download was needed. Build: 3.7 s, 19.2 MB component. |
| (b) prebuilt CPython/wasm build | **Not attempted: unreachable.** Every candidate source (python.org's WASI artifacts via GitHub releases, `vmware-labs/webassembly-language-runtimes`, `bytecodealliance/wasmtime` releases) is on github.com, which the egress policy blocks (403 on CONNECT, see proxy status log). Not needed once (a) worked. |
| (c) Rust guest with an embedded interpreter | **Not needed.** Python-in-WASM was achieved via (a). |

Host: `wasmtime::component::bindgen!` on the WIT, `wasmtime_wasi::p2::
add_to_linker_sync`, `WasiCtxBuilder::new().inherit_stderr()` (no preopened
directories, no sockets). Cancellation uses epoch interruption: the store has
`set_epoch_deadline(1)`, a host thread calls `engine.increment_epoch()` after
500 ms, the call returns `Trap::Interrupt`, and the instance is discarded and
re-instantiated. The host also measures cold cranelift compile versus loading a
precompiled `.cwasm`, which is what a real loader would cache.

```
cd spikes/ext-mechanism/proto-b
./build-guest.sh                                    # uv venv + componentize-py -> repl.wasm
(cd guest-py && ../.venv/bin/componentize-py -d ../wit -w repl-tool componentize app_full -o ../repl-fullstd.wasm)
cd host && cargo build --release
./target/release/proto-b-wasm-host --out ../../results/proto-b-wasm.json
./target/release/proto-b-wasm-host --wasm ../repl-fullstd.wasm --out ../../results/proto-b-wasm-fullstd.json
./target/release/proto-b-wasm-host --eval "import sys; sys.path"     # ad-hoc probe
```

State persistence, structured errors, cancellation and restart all worked in B
as well. What broke is below.

## Measurements

All latencies are host-side wall clock around one call, 200 calls per row
(20 for the 1 MB row, 5 for the CPU rows). "A" is the process prototype with the
numpy venv (system-Python numbers are within noise of these; both JSON files are
in `results/`). "B" is the default component; "B fullstd" is the variant that
pre-imports a broad stdlib slice at build time (see "What broke").

### Startup

| | A (process) | B (wasm) | B fullstd |
|---|---|---|---|
| Spawn/instantiate to first successful call, median of 5 | **22.0 ms** (19.5–23.4) | **0.7 ms** instantiate + 0.5 ms first call | 0.7 ms + 0.6 ms |
| One-time compile of the component (cranelift, 4 cores) | n/a | 3.0 s cold; **3.5 ms** from precompiled `.cwasm` | 3.2 s; 3.4 ms |
| Restart after a cancelled call | 22–23 ms | 4.8 ms | 4.5 ms |
| Artifact | 111-line `.py` + shared interpreter | 19.2 MB `.wasm`, 36.0 MB `.cwasm` | 23.4 MB / 40.4 MB |
| Build step to author the tool | none | componentize-py, 3.7 s | 7.0 s |
| Resident memory | child 13.4 MB | host 352 → 373 MB* | host 411 → 437 MB* |

\* Host RSS includes the wasmtime runtime and the in-process cold compile; it
was not separated from the instance's own footprint. Treat as an upper bound.

### Per-call latency (microseconds)

| Call | A p50 / p95 / p99 | B p50 / p95 / p99 | B fullstd p50 / p95 / p99 |
|---|---|---|---|
| Pure round-trip, no Python work (`ping` / `info`) | **52** / 69 / 87 | **2** / 3 / 5 | 7 / 11 / 33 |
| `eval("1+1")` | **74** / 99 / 152 | **55** / 93 / 110 | 57 / 97 / 128 |
| `eval` returning a 1 MB string | 7,063 / 8,337 / 9,309 | 3,756 / 4,643 / 4,684 | 3,687 / 4,062 / 5,113 |

The transport costs ~50 µs per call in A (pipe write, read, JSON, tokio wake)
and ~2–7 µs in B. Once a real `eval` is involved, Python's own parse/compile/exec
dominates and the two are within 20 µs of each other. Both are three to four
orders of magnitude below a model turn.

### CPU-bound work inside the tool (milliseconds, p50 of 5)

| Workload | A (native CPython 3.11) | B (CPython 3.14 in wasmtime) | Ratio |
|---|---|---|---|
| `sum(i*i for i in range(2_000_000))` | 71 | 504 | **7.1x** |
| 500k-iteration float loop with `math.sqrt` | 41 | 153 | **3.7x** |
| 300k `dict[str]` inserts + length sum | 106 | 235 | **2.2x** |
| `import numpy; np.arange(1e6).sum()` | 67 (first), 25 (reuse) | not possible | — |

### Cancellation

| | A | B |
|---|---|---|
| Mechanism | deadline → `SIGTERM` → (1 s) → `SIGKILL` | deadline → `engine.increment_epoch()` → `Trap::Interrupt` |
| Observed | killed by signal 15, 0.8–1.2 ms after the deadline | trapped 1 ms after the deadline |
| State after | process gone; respawn 22 ms; namespace fresh | store unusable after trap; re-instantiate 4.5 ms; namespace fresh |

Neither mechanism preserves state across a cancel. That is the right semantics
for D15 (a `Session` tool that was killed mid-call restarts clean).

### Facilities available to tool code

Probe: import a list of stdlib modules and try a handful of OS operations from
inside the REPL. A ran under a scrubbed environment; B had no preopened dirs.

| Facility | A (process) | B (wasm, default build) | B fullstd |
|---|---|---|---|
| `json`, `statistics`, `decimal`, `csv`, `pathlib`, `tempfile`, `pickle`, `struct`, `hashlib`, `asyncio`, `xml.etree` | ok | **ModuleNotFoundError** (not snapshotted) | ok (pre-imported at build) |
| `ssl`, `ctypes`, `mmap`, `fcntl` | ok | missing | **missing from the WASI build** (`_ssl`, `_ctypes` absent) |
| `subprocess.run(["true"])` | ok | module missing | `OSError: wasi does not support processes` |
| `threading.Thread().start()` | ok | module missing | `RuntimeError: can't start new thread` |
| `socket.connect` | reaches the kernel (ECONNREFUSED) | module missing | `PermissionError` (no `Net` granted) — correct |
| `open("/tmp/x","w")`, `os.listdir("/")` | ok | `FileNotFoundError` (no preopens) — correct; a loader would preopen the `Fs` atoms | same |
| `time.sleep`, `os.getcwd`, `os.environ` | ok | ok | ok |
| numpy 2.4.6 | ok (venv) | no | no |

## What broke

1. **Most of the stdlib is absent at runtime in the default component.**
   componentize-py snapshots the interpreter after importing the app module; only
   modules imported at that point (plus built-in C modules such as `math`,
   `zlib`, `time`) exist afterwards. `sys.path` inside the guest lists
   `/python/lib/python3.14`, but nothing is mounted there. A REPL whose whole
   point is ad-hoc `import` therefore fails on `import json`. Workaround
   demonstrated in `app_full.py`: import the stdlib slice you want at build time
   (+4 MB, +3 s build). Alternative not tested here: preopen a directory holding
   the 3.14 stdlib at `/python/lib/python3.14` (the stdlib ships inside the
   componentize-py wheel and is not written to disk, so there was nothing to
   mount without GitHub access to a CPython/WASI release).
2. **numpy cannot be used from the component.** Bundling the Linux wheel's
   `site-packages` "succeeds" silently (the component grows by 14 KB and
   `import numpy` fails at runtime); importing numpy at build time fails with
   `ModuleNotFoundError: No module named 'numpy._core._multiarray_umath'` —
   the extension modules are x86_64 `.so` files. PyPI publishes no wasi/wasm
   wheels for numpy (checked all 66 files of numpy 2.5.2). Community
   wasm32-wasi builds exist on GitHub (`dicej/wasi-wheels`) but github.com is
   blocked here, and in any case no such builds exist for gmsh, VTK/pyvista, or
   any in-house solver interface.
3. **No processes, no threads inside WASI.** `subprocess` raises `OSError:
   wasi does not support processes`. The domain tool needs to shell out to
   meshers and to `sbatch` (D6 grants `Proc{sbatch}` explicitly); a WASM tool
   cannot exercise that capability at all.
4. **Interpreted CPU work is 2–7x slower** in the wasm interpreter build.
5. **`/usr/bin/time` is not installed**; `build-guest.sh` uses bash `time`.
6. Not a failure, but noted: a cold cranelift compile of the 19 MB component
   takes 3 s and allocates a few hundred MB in the host process; a loader must
   cache `.cwasm` artifacts keyed by wasmtime version to get the 3.5 ms path.

## Scoring matrix

Scale: ++ clearly better, + adequate, − weaker, −− blocking for the domain.

| Criterion | A: process + JSON-RPC | B: WASM component | Notes |
|---|---|---|---|
| **Authorable by the agent at runtime, no recompile** | **++** The tool *is* a Python file the agent writes; the loader spawns it. Any language that can read stdin works. | **−** No host recompile either, but a 43 MB build tool, a 4–7 s build, and a 19–23 MB artifact per tool. The author must predict every module the tool will import. | Verified on both prototypes. |
| **Sandbox compatibility under P0.1's inner bwrap** | **+** One bwrap per session process is exactly the D5 shape; stdio passes through bwrap unchanged; `Fs`/`Net`/`Proc` atoms map to `--ro-bind`/`--bind`/`--unshare-net`/PATH. Caveat: under `--unshare-pid` the tool is PID 1 in its namespace and ignores an unhandled SIGTERM, so the launcher must escalate to SIGKILL (prototype already does) or the tool must install a handler. **Not run under bwrap here.** | **++** on isolation: wasmtime's capability sandbox needs no user namespaces, and preopens map one-to-one onto `Fs` atoms. **−** on shape: the instance runs *inside the host process*, so honoring D5's "out of process" means spawning a wasmtime host per session under bwrap anyway — i.e. option A plus a runtime. | Reasoned, not measured. If P0.1 fails, WASM is the only inner-sandbox option that needs no kernel features. |
| **State persistence across calls** | **++** namespace lives with the process; lost on cancel; 22 ms restart. | **++** module globals live with the instance; lost on trap; 4.5 ms restart. | Tie. |
| **Latency per call** | **+** ~50 µs transport, 74 µs `eval("1+1")`, 7 ms for a 1 MB result. | **++** ~2–7 µs transport, 55 µs `eval("1+1")`, 3.7 ms for 1 MB. | Both irrelevant against model turns. B's CPU-bound penalty (2–7x) is what matters, and it goes the other way. |
| **Packaging and distribution** | **++** Interpreter is part of the sandbox image (already needed for the `python` tool); dependencies via `uv` into a venv; numpy, meshio, gmsh, pyvista are ordinary wheels; tool = `.py` + `requirements`. | **−−** Each tool embeds its own CPython (19 MB) plus a 36 MB precompiled cache per wasmtime version. numpy only via out-of-band wasi wheels; meshing/VTK/solver bindings have no WASI builds; no `subprocess` so no shelling out to meshers or `sbatch`. | Decisive for dev plan §7. |
| Startup | + 22 ms | ++ 0.7 ms (with precompiled cache; 3 s without) | Irrelevant at session granularity. |
| Cancellation (D15) | + SIGTERM/SIGKILL, ~1 ms | ++ epoch trap, ~1 ms, no signals involved | Both fine. |
| Plumbing cost | 284-line host, 111-line guest, 4 crates | 259-line host, 54-line guest + 23-line WIT, wasmtime tree (1 GB `target/`, 29 MB host binary vs 1.4 MB) | Comparable in code; very different in build weight. |

## Recommendation

**Choose A: one long-lived sandboxed process per `Session` tool, calls as
newline-delimited JSON-RPC 2.0 over stdio.** The WASM prototype worked further
than expected — Python 3.14 in a component with persistent state, microsecond
calls, millisecond restarts, and a sandbox that needs no kernel features — but
it fails the domain on the criteria that matter: the agent's primary tool needs
numpy, meshing libraries, and the ability to run meshers and `sbatch`, none of
which exist inside WASI, and interpreted work runs 2–7x slower. Its wins
(≈50 µs per call, 20 ms per restart) are invisible next to a model turn.

Consequences to carry into the ADR:

- **P1.7 Session launcher:** spawn the tool process under the derived bwrap
  policy with stdio piped, scrubbed env (D10), a per-call deadline, and
  SIGTERM → grace → SIGKILL on cancel (D15). Verify SIGTERM delivery under
  `--unshare-pid` on the real stack (P0.1 follow-up); until then the escalation
  to SIGKILL is mandatory, not optional.
- **P2.1 manifest + loader:** a tool is a directory with a manifest naming the
  command to spawn, the tool schema, its capability atoms, and its
  dependencies; the loader creates the venv (uv) at install time, not per call.
  The wire format should be the stdio-MCP shape of JSON-RPC (`initialize`,
  `tools/list`, `tools/call`) or a strict subset of it, so the same launcher
  serves agent-authored tools and P2.2 MCP servers; the exact schema is
  ADR-0004's call.
- **Large results:** the 1 MB case costs 7 ms; results above the cap should
  spill to the artifact store (D12) rather than travel inline, and binary
  payloads (meshes, images) should be written to the sandboxed workspace and
  returned as handles, not base64.
- **Keep WASM in reserve** as a *second* out-of-process kind for pure-compute,
  dependency-free, untrusted tools, or as the fallback inner sandbox if the
  P0.1 bwrap nesting fails on the login node. Nothing in the A design precludes
  adding it later behind the same manifest.

## What a human with more access should re-run

1. Run `proto-a` under the real inner bwrap policy from P0.1 (inside the Podman
   uid map of D18) and confirm (a) stdio JSON-RPC passes through unchanged,
   (b) SIGTERM reaches the Python process — or does not, under
   `--unshare-pid` — and the SIGKILL escalation restores the session.
2. If WASM is ever revisited: with GitHub access, fetch a wasm32-wasi numpy
   from `dicej/wasi-wheels` and check whether componentize-py can bundle it;
   also test preopening a CPython 3.14 stdlib directory at
   `/python/lib/python3.14` to remove the pre-import requirement. Neither
   changes the `subprocess`/meshing conclusion.
3. Repeat the CPU rows on the HPC login node; the 4-vCPU container here is
   not representative of absolute numbers, only ratios.

## Checkbox status for `docs/IMPLEMENTATION_PLAN.md` P0.3

Work is complete for all four items; per CONTRIBUTING they are ticked on merge:

- Prototype A: built and measured here.
- Prototype B: built and measured here with a real Python component (route a).
- Scoring: done above; the bwrap-compatibility row is reasoned, not executed,
  because `bwrap` is not available in this environment.
- Write-up: this document. ADR-0001 is drafted as `proposed` for P0.4.
