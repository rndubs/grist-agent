#!/usr/bin/env bash
# Reports which backend test tiers are available in this environment (P0.5).
#
#   standin/env-gate.sh                 print a table, exit 0
#   standin/env-gate.sh --require standin,vllm   exit 1 unless every listed tier is available
#   standin/env-gate.sh --github-output          also append tier_<name>=<status> to $GITHUB_OUTPUT
#
# Tiers and the variables that gate them (tests skip when unset; CI never sets
# the real-backend ones):
#   standin  STANDIN_LITELLM_BASE_URL / STANDIN_OPENAI_BASE_URL (+ STANDIN_LITELLM_KEY)
#   vllm     GRIST_VLLM_BASE_URL        [GRIST_VLLM_API_KEY]
#   litellm  GRIST_LITELLM_BASE_URL     GRIST_LITELLM_API_KEY
#   slurm    GRIST_REAL_SLURM=1 and a real sbatch on PATH
set -euo pipefail
here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
require=""; gh_out=0
while (($# > 0)); do
  case "$1" in
    --require) require="$2"; shift 2 ;;
    --require=*) require="${1#*=}"; shift ;;
    --github-output) gh_out=1; shift ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "env-gate.sh: unknown option $1" >&2; exit 2 ;;
  esac
done
timeout=${GATE_TIMEOUT:-5}

probe() {  # probe URL [KEY] -> 0 if HTTP 2xx
  local url="$1" key="${2-}" code
  if [[ -n "$key" ]]; then
    code=$(curl -sS -m "$timeout" -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $key" "$url" 2>/dev/null || echo 000)
  else
    code=$(curl -sS -m "$timeout" -o /dev/null -w '%{http_code}' "$url" 2>/dev/null || echo 000)
  fi
  [[ $code == 2* ]]
}

declare -A status detail

# -- standin ------------------------------------------------------------------
lb=${STANDIN_LITELLM_BASE_URL:-http://127.0.0.1:4000/v1}
ob=${STANDIN_OPENAI_BASE_URL:-http://127.0.0.1:8080/v1}
key=${STANDIN_LITELLM_KEY-}
if [[ -z "$key" && -f "$here/.env" ]]; then key=$(sed -n 's/^LITELLM_MASTER_KEY=//p' "$here/.env" | tail -1); fi
if probe "$ob/models" && probe "$lb/models" "$key"; then
  status[standin]=available; detail[standin]="llama.cpp $ob, LiteLLM $lb"
else
  status[standin]=unreachable; detail[standin]="run standin/up.sh (llama.cpp $ob, LiteLLM $lb)"
fi

# -- vllm ----------------------------------------------------------------------
if [[ -n "${GRIST_VLLM_BASE_URL-}" ]]; then
  if probe "${GRIST_VLLM_BASE_URL%/}/models" "${GRIST_VLLM_API_KEY-}"; then status[vllm]=available; detail[vllm]="$GRIST_VLLM_BASE_URL"
  else status[vllm]=unreachable; detail[vllm]="GRIST_VLLM_BASE_URL=$GRIST_VLLM_BASE_URL did not answer /models"; fi
else status[vllm]=skipped; detail[vllm]="GRIST_VLLM_BASE_URL unset"; fi

# -- litellm (real upstreams) --------------------------------------------------
if [[ -n "${GRIST_LITELLM_BASE_URL-}" && -n "${GRIST_LITELLM_API_KEY-}" ]]; then
  if probe "${GRIST_LITELLM_BASE_URL%/}/models" "$GRIST_LITELLM_API_KEY"; then status[litellm]=available; detail[litellm]="$GRIST_LITELLM_BASE_URL"
  else status[litellm]=unreachable; detail[litellm]="GRIST_LITELLM_BASE_URL=$GRIST_LITELLM_BASE_URL rejected or unreachable"; fi
elif [[ -n "${GRIST_LITELLM_BASE_URL-}" ]]; then status[litellm]=skipped; detail[litellm]="GRIST_LITELLM_API_KEY unset"
else status[litellm]=skipped; detail[litellm]="GRIST_LITELLM_BASE_URL unset"; fi

# -- real slurm ----------------------------------------------------------------
if [[ "${GRIST_REAL_SLURM-}" == 1 ]]; then
  if ! command -v sbatch >/dev/null 2>&1; then status[slurm]=unreachable; detail[slurm]="GRIST_REAL_SLURM=1 but no sbatch on PATH"
  elif sbatch --version 2>/dev/null | grep -q fake; then status[slurm]=misconfigured; detail[slurm]="GRIST_REAL_SLURM=1 but PATH has the fake sbatch ($(command -v sbatch))"
  else status[slurm]=available; detail[slurm]="$(sbatch --version 2>/dev/null | head -1) at $(command -v sbatch)"; fi
else status[slurm]=skipped; detail[slurm]="GRIST_REAL_SLURM unset"; fi

printf '%-9s %-14s %s\n' TIER STATUS DETAIL
for t in standin vllm litellm slurm; do
  printf '%-9s %-14s %s\n' "$t" "${status[$t]}" "${detail[$t]}"
  if (( gh_out )) && [[ -n "${GITHUB_OUTPUT-}" ]]; then echo "tier_$t=${status[$t]}" >>"$GITHUB_OUTPUT"; fi
done

rc=0
IFS=, read -r -a req <<<"$require"
for t in "${req[@]+"${req[@]}"}"; do
  [[ -n "$t" ]] || continue
  if [[ "${status[$t]-missing}" != available ]]; then echo "env-gate: required tier '$t' is ${status[$t]-unknown}" >&2; rc=1; fi
done
exit $rc
