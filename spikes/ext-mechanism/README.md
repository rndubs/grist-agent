# P0.3 spike: out-of-process tool mechanism

Throwaway code. The deliverables are `docs/spikes/extension-mechanism.md` and
`docs/adr/0001-out-of-process-tool-mechanism.md`. Nothing here is imported by a crate.

Test case (D5, dev plan §7): a persistent Python REPL as a `Session`-kind tool.

| Dir | What |
|---|---|
| `proto-a/` | Rust (tokio) host spawns `repl_server.py`; each call is a JSON-RPC 2.0 line over stdio. SIGTERM cancel, restart, latency, numpy via a plain venv. |
| `proto-b/` | Same REPL as a WASM component: `wit/repl.wit`, `guest-py/app.py` built by componentize-py, `host/` is a wasmtime 47 host with epoch-based cancel. `guest-py/app_full.py` and `app_numpy.py` are variants used to find what breaks. |
| `results/` | JSON output of the last run of each host; the write-up's tables are copied from these. |
| `run_all.sh` | Rebuilds and re-measures everything. |

Both Cargo projects carry their own `[workspace]` table so they never attach to the root workspace.
