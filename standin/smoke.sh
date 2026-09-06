#!/usr/bin/env bash
# Smoke test for the CI stand-in stack (P0.5). Run after `standin/up.sh`.
#
#   standin/smoke.sh [all|models|tools|stream|slurm]
#
# (a) tools  : chat completion with a tool definition through LiteLLM at
#              stand-in/<model>; asserts a non-empty `tool_calls` array (3 attempts)
# (b) stream : same directly against llama.cpp with stream=true; asserts SSE
#              `data:` lines and a terminating `data: [DONE]`
# (c) slurm  : fake sbatch runs the mock solver and the epilog fires (inside the
#              compose `slurm` service, or locally with STANDIN_SLURM_EXEC=local)
#
# Environment (defaults match compose.yaml's host ports; CI exports the same):
#   STANDIN_OPENAI_BASE_URL   http://127.0.0.1:8080/v1     (llama.cpp)
#   STANDIN_LITELLM_BASE_URL  http://127.0.0.1:4000/v1     (LiteLLM)
#   STANDIN_LITELLM_KEY       LITELLM_MASTER_KEY from standin/.env if unset
#   STANDIN_MODEL             qwen2.5-1.5b-instruct
#   STANDIN_SLURM_EXEC        "docker compose -f <standin>/compose.yaml exec -T slurm" | "local"
#   STANDIN_TOOL_ATTEMPTS     3
set -euo pipefail

here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
: "${STANDIN_OPENAI_BASE_URL:=http://127.0.0.1:8080/v1}"
: "${STANDIN_LITELLM_BASE_URL:=http://127.0.0.1:4000/v1}"
: "${STANDIN_MODEL:=qwen2.5-1.5b-instruct}"
: "${STANDIN_TOOL_ATTEMPTS:=3}"
if [[ -z "${STANDIN_LITELLM_KEY-}" && -f "$here/.env" ]]; then
  STANDIN_LITELLM_KEY=$(sed -n 's/^LITELLM_MASTER_KEY=//p' "$here/.env" | tail -1)
fi
: "${STANDIN_LITELLM_KEY:?STANDIN_LITELLM_KEY is unset and standin/.env has no LITELLM_MASTER_KEY}"
: "${STANDIN_SLURM_EXEC:=docker compose -f $here/compose.yaml exec -T slurm}"
export STANDIN_OPENAI_BASE_URL STANDIN_LITELLM_BASE_URL STANDIN_LITELLM_KEY STANDIN_MODEL

part=${1:-all}
tmp=$(mktemp -d "${TMPDIR:-/tmp}/standin-smoke.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
fail=0
section() { printf '\n== %s\n' "$1"; }
ok()  { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fail=1; }

curl_json() {  # curl_json URL KEY BODYFILE OUTFILE -> http status
  curl -sS --max-time 600 -o "$4" -w '%{http_code}' \
    -H "Authorization: Bearer $2" -H "Content-Type: application/json" \
    -X POST "$1" --data-binary "@$3"
}

# ---------------------------------------------------------------------------
if [[ $part == all || $part == models ]]; then
  section "model listing"
  st=$(curl -sS --max-time 30 -o "$tmp/models.json" -w '%{http_code}' \
    -H "Authorization: Bearer $STANDIN_LITELLM_KEY" "$STANDIN_LITELLM_BASE_URL/models" || echo 000)
  if [[ $st == 200 ]] && python3 - "$tmp/models.json" "stand-in/$STANDIN_MODEL" <<'PY'
import json, sys
ids = [m.get("id") for m in json.load(open(sys.argv[1])).get("data", [])]
print("  LiteLLM models:", ids)
sys.exit(0 if sys.argv[2] in ids else 1)
PY
  then ok "LiteLLM lists stand-in/$STANDIN_MODEL"; else bad "LiteLLM /models (HTTP $st) does not list stand-in/$STANDIN_MODEL"; fi

  st=$(curl -sS --max-time 30 -o "$tmp/models2.json" -w '%{http_code}' "$STANDIN_OPENAI_BASE_URL/models" || echo 000)
  if [[ $st == 200 ]] && python3 -c 'import json,sys; ids=[m["id"] for m in json.load(open(sys.argv[1]))["data"]]; print("  llama.cpp models:", ids); sys.exit(0 if sys.argv[2] in ids else 1)' "$tmp/models2.json" "$STANDIN_MODEL"
  then ok "llama.cpp lists alias $STANDIN_MODEL"; else bad "llama.cpp /models (HTTP $st) does not list $STANDIN_MODEL"; fi
fi

# ---------------------------------------------------------------------------
if [[ $part == all || $part == tools ]]; then
  section "(a) tool call through LiteLLM: stand-in/$STANDIN_MODEL"
  got=0
  for attempt in $(seq 1 "$STANDIN_TOOL_ATTEMPTS"); do
    python3 - "$tmp/tools-req.json" "stand-in/$STANDIN_MODEL" "$attempt" <<'PY'
import json, sys
out, model, attempt = sys.argv[1], sys.argv[2], int(sys.argv[3])
req = {
  "model": model,
  "messages": [
    {"role": "system", "content": "You are a weather assistant. You must call the get_weather function to answer any question about the weather. Never answer from memory."},
    {"role": "user", "content": "What is the weather in Paris right now? Call get_weather for Paris."},
  ],
  "tools": [{
    "type": "function",
    "function": {
      "name": "get_weather",
      "description": "Get the current weather for a city.",
      "parameters": {
        "type": "object",
        "properties": {
          "city": {"type": "string", "description": "City name, e.g. Paris"},
          "unit": {"type": "string", "enum": ["celsius", "fahrenheit"]},
        },
        "required": ["city"],
      },
    },
  }],
  "tool_choice": "auto",
  "temperature": 0.0 if attempt == 1 else 0.6,
  "seed": 41 + attempt,
  "max_tokens": 256,
}
json.dump(req, open(out, "w"))
PY
    st=$(curl_json "$STANDIN_LITELLM_BASE_URL/chat/completions" "$STANDIN_LITELLM_KEY" "$tmp/tools-req.json" "$tmp/tools-resp.json" || echo 000)
    if [[ $st != 200 ]]; then echo "  attempt $attempt: HTTP $st: $(head -c 400 "$tmp/tools-resp.json")"; continue; fi
    if python3 - "$tmp/tools-resp.json" <<'PY'
import json, sys
r = json.load(open(sys.argv[1]))
msg = r["choices"][0]["message"]
calls = msg.get("tool_calls") or []
print("  finish_reason:", r["choices"][0].get("finish_reason"),
      "| reasoning_content present:", "reasoning_content" in msg and msg["reasoning_content"] is not None,
      "| usage:", r.get("usage"))
if not isinstance(calls, list) or not calls:
    print("  no tool_calls; content was:", repr((msg.get("content") or "")[:300]))
    sys.exit(1)
c = calls[0]
print("  tool_calls[0]:", json.dumps(c))
assert c.get("type", "function") == "function"
assert c["function"]["name"] == "get_weather", c["function"]["name"]
args = json.loads(c["function"]["arguments"])   # must be a JSON object string
assert isinstance(args, dict) and "city" in args, args
if "paris" not in str(args.get("city", "")).lower():
    print("  warning: city argument is not Paris:", args)
PY
    then got=1; ok "tool_calls array returned (attempt $attempt)"; break
    else echo "  attempt $attempt: no valid tool call"; fi
  done
  (( got )) || bad "no tool call after $STANDIN_TOOL_ATTEMPTS attempts"
fi

# ---------------------------------------------------------------------------
if [[ $part == all || $part == stream ]]; then
  section "(b) streaming directly against llama.cpp: $STANDIN_MODEL"
  python3 - "$tmp/stream-req.json" "$STANDIN_MODEL" <<'PY'
import json, sys
json.dump({
  "model": sys.argv[2],
  "messages": [{"role": "user", "content": "Reply with exactly: hello from the stand-in"}],
  "stream": True, "stream_options": {"include_usage": True},
  "temperature": 0, "max_tokens": 32,
}, open(sys.argv[1], "w"))
PY
  st=$(curl -sS -N --max-time 300 -D "$tmp/stream-headers.txt" -o "$tmp/stream.txt" -w '%{http_code}' \
    -H "Content-Type: application/json" -X POST "$STANDIN_OPENAI_BASE_URL/chat/completions" \
    --data-binary "@$tmp/stream-req.json" || echo 000)
  if [[ $st == 200 ]]; then ok "HTTP 200"; else bad "HTTP $st: $(head -c 300 "$tmp/stream.txt")"; fi
  if grep -qi '^content-type: *text/event-stream' "$tmp/stream-headers.txt"; then ok "Content-Type is text/event-stream"; else bad "unexpected Content-Type: $(grep -i '^content-type' "$tmp/stream-headers.txt" || echo none)"; fi
  n=$(grep -c '^data: {' "$tmp/stream.txt" || true)
  if (( n >= 1 )); then ok "$n SSE data: chunks"; else bad "no 'data: {' lines"; fi
  if grep -q '^data: \[DONE\]' "$tmp/stream.txt"; then ok "terminated with data: [DONE]"; else bad "missing data: [DONE]"; fi
  if python3 - "$tmp/stream.txt" <<'PY'
import json, sys
text, reasoning, usage, roles = "", "", None, set()
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    line = line.rstrip("\n")
    if not line.startswith("data: ") or line == "data: [DONE]":
        continue
    chunk = json.loads(line[6:])
    if chunk.get("usage"):
        usage = chunk["usage"]
    for ch in chunk.get("choices", []):
        d = ch.get("delta", {})
        text += d.get("content") or ""
        reasoning += d.get("reasoning_content") or ""
        if d.get("role"): roles.add(d["role"])
print("  streamed text:", repr(text[:120]))
print("  reasoning_content streamed:", bool(reasoning), "| usage in final chunk:", usage is not None, "| roles:", sorted(roles))
sys.exit(0 if text.strip() else 1)
PY
  then ok "delta.content assembled into non-empty text"; else bad "no delta.content in stream"; fi
fi

# ---------------------------------------------------------------------------
if [[ $part == all || $part == slurm ]]; then
  section "(c) fake sbatch runs the mock solver, epilog fires"
  if [[ $STANDIN_SLURM_EXEC == local ]]; then
    export PATH="$here/solver/opt/acme-solver/bin:$here/slurm/bin:$PATH"
    export FAKE_SLURM_STATE_DIR="$tmp/fake-slurm-state"
    ( cd "$tmp" && "$here/slurm/libexec/sbatch-solver-check" ) && ok "local sbatch -> solver -> epilog" || bad "local sbatch/solver check failed"
  else
    $STANDIN_SLURM_EXEC /opt/fake-slurm/libexec/sbatch-solver-check && ok "in-container sbatch -> solver -> epilog" || bad "in-container sbatch/solver check failed"
  fi
fi

echo
if (( fail )); then echo "standin smoke: FAILED"; exit 1; else echo "standin smoke: all checks passed"; fi
