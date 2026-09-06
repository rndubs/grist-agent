#!/usr/bin/env bash
# Reproduce every measurement in docs/spikes/extension-mechanism.md.
# Needs: rustc 1.94.1, python3.11, uv (UV env var or /root/.local/bin/uv), network to crates.io and PyPI once.
set -euo pipefail
cd "$(dirname "$0")"
UV=${UV:-/root/.local/bin/uv}
mkdir -p results

echo "## Prototype A: process + JSON-RPC over stdio"
(cd proto-a && cargo build --release -q)
[ -d proto-a/.venv ] || { $UV venv -q -p 3.11 proto-a/.venv && $UV pip install -q -p proto-a/.venv/bin/python numpy; }
proto-a/target/release/proto-a-jsonrpc --out results/proto-a-system-python.json
proto-a/target/release/proto-a-jsonrpc --python proto-a/.venv/bin/python --out results/proto-a-venv-numpy.json

echo "## Prototype B: WASM component (componentize-py) in wasmtime"
proto-b/build-guest.sh
(cd proto-b/guest-py && ../.venv/bin/componentize-py -d ../wit -w repl-tool componentize app_full -o ../repl-fullstd.wasm)
(cd proto-b/host && cargo build --release -q)
proto-b/host/target/release/proto-b-wasm-host --out results/proto-b-wasm.json
proto-b/host/target/release/proto-b-wasm-host --wasm proto-b/repl-fullstd.wasm --out results/proto-b-wasm-fullstd.json

echo "## What breaks: numpy at build time (expected to fail)"
(cd proto-b/guest-py && ../.venv/bin/componentize-py -d ../wit -w repl-tool componentize app_numpy \
   -p . -p ../../proto-a/.venv/lib/python3.11/site-packages -o /dev/null 2>&1 | grep -E "^(ImportError|ModuleNotFoundError)" || true)
