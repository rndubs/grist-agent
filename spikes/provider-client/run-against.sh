#!/usr/bin/env bash
# P0.2 provider spike runner.
#
#   run-against.sh fake              # no network: fake upstream, all four shapes
#   run-against.sh standin           # CI stand-in stack: llama.cpp direct + LiteLLM proxy
#   run-against.sh standin-llamacpp  # just the llama.cpp endpoint
#   run-against.sh standin-litellm   # just the LiteLLM proxy
#   run-against.sh vllm              # human: real vLLM
#   run-against.sh litellm           # human: real LiteLLM with keys
#
# Environment:
#   standin*:  STANDIN_OPENAI_BASE_URL  STANDIN_MODEL
#              STANDIN_LITELLM_BASE_URL STANDIN_LITELLM_KEY [STANDIN_LITELLM_MODEL=stand-in/$STANDIN_MODEL]
#   vllm:      GRIST_VLLM_BASE_URL  GRIST_VLLM_MODEL  [GRIST_VLLM_API_KEY]
#   litellm:   GRIST_LITELLM_BASE_URL  GRIST_LITELLM_API_KEY  GRIST_LITELLM_MODEL (e.g. anthropic/claude-sonnet-4-5)
#   all:       [OUT=out] output dir; [OPTIONAL=structured] comma list of informational scenarios;
#              [PRESET=...] override the quirk preset; [CARGO_PROFILE=debug|release]
#
# Runs, per endpoint: plain completion (stream + non-stream), reasoning capture,
# two-turn tool call, structured-output probe, then `probe`, which prints the
# observed quirk row. Rows are appended to $OUT/quirks.md, JSON to $OUT/<label>.json.
# Exit code is non-zero if a required scenario failed on any endpoint.
set -euo pipefail
# Absolute path to this script: the `standin` target re-invokes it after the cd below,
# and $0 may be relative to the caller's directory (as in CI).
self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
cd "$(dirname "$self")"

target="${1:-}"
[ -n "$target" ] || { sed -n '2,20p' "$0"; exit 64; }
OUT="${OUT:-out}"
mkdir -p "$OUT"
: > "$OUT/quirks.md"
profile="${CARGO_PROFILE:-debug}"
if [ "$profile" = release ]; then cargo build -q --release; else cargo build -q; fi
BIN="./target/$profile/provider-spike"

with_v1() { # ensure the base URL ends in /v1 (the client appends /chat/completions)
  local u="${1%/}"
  case "$u" in */v1) echo "$u" ;; *) echo "$u/v1" ;; esac
}

failures=0
run_one() { # label preset base_url model [api_key_env] [optional-scenarios]
  local label="$1" preset="${PRESET:-$2}" base model="$4" keyenv="${5:-}" optional="${6:-${OPTIONAL:-structured}}"
  base="$(with_v1 "$3")"
  local args=(--preset "$preset" --base-url "$base" --model "$model")
  [ -n "$keyenv" ] && args+=(--api-key-env "$keyenv")
  echo "=================================================================="
  echo "== $label   preset=$preset   $base   model=$model   key_env=${keyenv:-none}"
  echo "=================================================================="
  local rc=0
  echo "--- complete (stream)";      "$BIN" "${args[@]}" complete            || rc=1
  echo "--- complete (non-stream)";  "$BIN" "${args[@]}" complete --no-stream || rc=1
  echo "--- reasoning";              "$BIN" "${args[@]}" reasoning           || echo "(reasoning scenario failed; required unless listed in OPTIONAL)"
  echo "--- tools (two-turn)";       "$BIN" "${args[@]}" tools               || rc=1
  echo "--- structured probe";       "$BIN" "${args[@]}" structured          || echo "(structured output not honored: recorded as a flag)"
  echo "--- probe -> quirk row"
  if ! "$BIN" "${args[@]}" probe --label "$label" --optional "$optional" --out "$OUT/$label.json" | tee "$OUT/$label.md"; then rc=1; fi
  cat "$OUT/$label.md" >> "$OUT/quirks.md"
  if [ $rc -ne 0 ]; then echo "!! $label: required scenario failed"; failures=$((failures+1)); fi
}

case "$target" in
  fake)
    "$BIN" fake --port 0 > "$OUT/fake.log" 2>&1 &
    fake_pid=$!
    trap 'kill $fake_pid 2>/dev/null || true' EXIT
    for _ in $(seq 1 50); do grep -q "listening on" "$OUT/fake.log" 2>/dev/null && break; sleep 0.1; done
    addr="$(sed -n 's|.*listening on http://\([^ ]*\).*|\1|p' "$OUT/fake.log")"
    [ -n "$addr" ] || { cat "$OUT/fake.log"; exit 1; }
    # The fake's LiteLLM shape insists on a bearer token like the real proxy.
    export GRIST_LITELLM_API_KEY="${GRIST_LITELLM_API_KEY:-sk-fake}"
    run_one fake-vllm     vllm     "http://$addr/vllm"     fake-model
    run_one fake-litellm  litellm  "http://$addr/litellm"  fake-model GRIST_LITELLM_API_KEY
    run_one fake-llamacpp llamacpp "http://$addr/llamacpp" fake-model
    run_one fake-hermes   hermes   "http://$addr/hermes"   fake-model
    ;;
  standin)
    rc1=0; "$self" standin-llamacpp || rc1=$?
    mv "$OUT/quirks.md" "$OUT/quirks-llamacpp.md"
    rc2=0; "$self" standin-litellm || rc2=$?
    cat "$OUT/quirks-llamacpp.md" "$OUT/quirks.md" > "$OUT/quirks-all.md" && mv "$OUT/quirks-all.md" "$OUT/quirks.md"
    exit $(( rc1 || rc2 ))
    ;;
  standin-llamacpp)
    : "${STANDIN_OPENAI_BASE_URL:?set by the P0.5 stand-in stack}" "${STANDIN_MODEL:?}"
    # Small stand-in models may not be reasoning models: reasoning is informational here.
    run_one standin-llamacpp llamacpp "$STANDIN_OPENAI_BASE_URL" "$STANDIN_MODEL" "" "${OPTIONAL:-structured,reasoning}"
    ;;
  standin-litellm)
    : "${STANDIN_LITELLM_BASE_URL:?set by the P0.5 stand-in stack}" "${STANDIN_LITELLM_KEY:?}" "${STANDIN_MODEL:?}"
    run_one standin-litellm litellm "$STANDIN_LITELLM_BASE_URL" "${STANDIN_LITELLM_MODEL:-stand-in/$STANDIN_MODEL}" STANDIN_LITELLM_KEY "${OPTIONAL:-structured,reasoning}"
    ;;
  vllm)
    : "${GRIST_VLLM_BASE_URL:?e.g. http://gpu-node:8000/v1}" "${GRIST_VLLM_MODEL:?served model name}"
    run_one vllm vllm "$GRIST_VLLM_BASE_URL" "$GRIST_VLLM_MODEL" "${GRIST_VLLM_API_KEY:+GRIST_VLLM_API_KEY}"
    ;;
  litellm)
    : "${GRIST_LITELLM_BASE_URL:?e.g. https://litellm.example/v1}" "${GRIST_LITELLM_API_KEY:?}" "${GRIST_LITELLM_MODEL:?provider/model as routed by the proxy}"
    run_one litellm litellm "$GRIST_LITELLM_BASE_URL" "$GRIST_LITELLM_MODEL" GRIST_LITELLM_API_KEY
    ;;
  *) echo "unknown target: $target" >&2; exit 64 ;;
esac

echo
echo "================ quirk rows ($OUT/quirks.md) ================"
grep -E '^\| [a-z0-9-]+ \((configured|observed)\)' "$OUT/quirks.md" || true
[ "$failures" -eq 0 ]
