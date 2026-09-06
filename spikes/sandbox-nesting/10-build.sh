#!/usr/bin/env bash
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd); mkdir -p "$here/results"
log="$here/results/$(hostname -s)-$(date +%Y%m%d).log"
exec > >(tee -a "$log") 2>&1
echo "=== 10-build $(date -u +%Y-%m-%dT%H:%M:%SZ) ==="
IMG=${IMG:-localhost/grist-bwrap-spike:latest}
# Site flags per D18.
set -x
podman build \
  --userns-uid-map=0:0:1 --userns-uid-map=1:1:1999 --userns-uid-map=65534:2000:2 \
  --userns-gid-map=0:0:1 --userns-gid-map=1:1:1999 --userns-gid-map=65534:2000:2 \
  -t "$IMG" -f "$here/Containerfile" "$here"
set +x
echo "PASS built $IMG"
echo "=== end 10-build ==="
