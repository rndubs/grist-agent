#!/usr/bin/env bash
# Bring up the CI stand-in stack and wait until every service is healthy.
#
#   standin/up.sh              start (creates standin/.env with a random master key if missing)
#   standin/up.sh --print-env  print `export STANDIN_*=...` lines for host-side clients
#   standin/up.sh --down       stop and remove containers and volumes (model cache is kept)
#
# Requires Docker Compose v2 (`docker compose`) or Podman with podman-compose
# (set COMPOSE="podman-compose").
set -euo pipefail

here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
cd "$here"
COMPOSE=${COMPOSE:-docker compose}

ensure_env() {
  if [[ ! -f .env ]]; then
    local key
    key="sk-standin-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
    printf 'LITELLM_MASTER_KEY=%s\n' "$key" >.env
    echo "standin/up.sh: wrote standin/.env with a fresh LITELLM_MASTER_KEY" >&2
  fi
  # shellcheck disable=SC1091
  set -a; source .env; set +a
}

print_env() {
  ensure_env
  cat <<ENV
export STANDIN_OPENAI_BASE_URL="http://127.0.0.1:${STANDIN_LLAMA_PORT:-8080}/v1"
export STANDIN_LITELLM_BASE_URL="http://127.0.0.1:${STANDIN_LITELLM_PORT:-4000}/v1"
export STANDIN_LITELLM_KEY="$LITELLM_MASTER_KEY"
export STANDIN_MODEL="${STANDIN_MODEL:-qwen2.5-1.5b-instruct}"
ENV
}

case "${1-}" in
  --print-env) print_env; exit 0 ;;
  --down) ensure_env; exec $COMPOSE -f compose.yaml down -v --remove-orphans ;;
  ""|--up) ;;
  *) echo "usage: up.sh [--print-env|--down]" >&2; exit 2 ;;
esac

ensure_env
mkdir -p "${STANDIN_MODEL_CACHE:-.cache/models}"
echo "standin/up.sh: pulling images and starting (first run downloads the GGUF, ~1.1 GB)" >&2
$COMPOSE -f compose.yaml up -d --wait --wait-timeout "${STANDIN_UP_TIMEOUT:-1500}" || {
  echo "standin/up.sh: stack did not become healthy; recent logs:" >&2
  $COMPOSE -f compose.yaml ps >&2 || true
  $COMPOSE -f compose.yaml logs --tail=60 >&2 || true
  exit 1
}
$COMPOSE -f compose.yaml ps >&2
echo "standin/up.sh: healthy. Host-side environment:" >&2
print_env
