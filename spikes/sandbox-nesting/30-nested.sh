#!/usr/bin/env bash
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd); mkdir -p "$here/results"
log="$here/results/$(hostname -s)-$(date +%Y%m%d).log"
exec > >(tee -a "$log") 2>&1
echo "=== 30-nested $(date -Is) VARIANT=${VARIANT:-default} ==="
echo "--- facts inside the container (before any bwrap)"
"$here/run-podman.sh" sh -c 'id; cat /proc/self/uid_map; cat /proc/sys/user/max_user_namespaces 2>&1; grep Seccomp /proc/self/status; ls -la /usr/bin/bwrap'
echo "--- podman -> outer bwrap -> inner bwrap battery"
if "$here/run-podman.sh" /usr/local/bin/outer.sh /usr/local/bin/inner-tool-call.sh; then
  echo "PASS nested bwrap battery (VARIANT=${VARIANT:-default})"
else
  echo "FAIL nested bwrap battery (VARIANT=${VARIANT:-default}); rerun with VARIANT=no-seccomp, then keep-ns, and record each"
fi
echo "=== end 30-nested ==="
