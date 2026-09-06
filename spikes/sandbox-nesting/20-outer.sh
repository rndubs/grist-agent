#!/usr/bin/env bash
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd); mkdir -p "$here/results"
log="$here/results/$(hostname -s)-$(date +%Y%m%d).log"
exec > >(tee -a "$log") 2>&1
echo "=== 20-outer $(date -Is) VARIANT=${VARIANT:-default} ==="
if "$here/run-podman.sh" /usr/local/bin/outer.sh sh -c 'echo "uid=$(id -u) pid=$$"; cat /proc/self/uid_map'; then
  echo "PASS podman -> outer bwrap"
else
  echo "FAIL podman -> outer bwrap (exit $?)"
fi
echo "=== end 20-outer ==="
