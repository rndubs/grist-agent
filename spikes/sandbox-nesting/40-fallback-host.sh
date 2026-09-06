#!/usr/bin/env bash
# Fallback: bwrap directly on the login node, no container (P0.1). Outer bwrap
# then inner bwrap using the same scripts, with $PWD/work as the repo.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd); mkdir -p "$here/results" "$here/work"
log="$here/results/$(hostname -s)-$(date +%Y%m%d).log"
exec > >(tee -a "$log") 2>&1
echo "=== 40-fallback-host $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
# Same shape as outer.sh but binding the spike's work dir to /work.
outer() {
  bwrap --unshare-user --unshare-pid --unshare-ipc --unshare-uts \
    --ro-bind / / --bind "$here/work" /work --tmpfs /tmp --tmpfs /scratch \
    --ro-bind "$here/inner-tool-call.sh" /usr/local/bin/inner-tool-call.sh \
    --proc /proc --dev /dev --die-with-parent --new-session -- "$@"
}
if outer /usr/local/bin/inner-tool-call.sh; then
  echo "PASS host bwrap -> bwrap battery"
else
  echo "FAIL host bwrap -> bwrap battery (exit $?)"
fi
echo "=== end 40-fallback-host ==="
