#!/usr/bin/env bash
# Build the Python REPL component with componentize-py. Needs network to PyPI once.
set -euo pipefail
cd "$(dirname "$0")"
UV=${UV:-/root/.local/bin/uv}
[ -d .venv ] || $UV venv -q -p 3.11 .venv
$UV pip install -q -p .venv/bin/python 'componentize-py==0.25.0'
.venv/bin/componentize-py --version
cd guest-py
TIMEFORMAT="componentize wall=%Rs"; time \
  ../.venv/bin/componentize-py -d ../wit -w repl-tool componentize app -o ../repl.wasm
ls -l ../repl.wasm
