#!/usr/bin/env bash
# Shared podman run invocation with the site uid map (D18) plus optional variants.
# VARIANT=default|no-seccomp|keep-ns|sys-admin  (see docs/spikes/sandbox-nesting.md)
set -euo pipefail
IMG=${IMG:-localhost/grist-bwrap-spike:latest}
VARIANT=${VARIANT:-default}
extra=()
case "$VARIANT" in
  default)    ;;
  no-seccomp) extra+=(--security-opt seccomp=unconfined) ;;
  keep-ns)    extra+=(--userns=keep-id) ;;
  sys-admin)  extra+=(--cap-add=SYS_ADMIN) ;;   # diagnostic only; NOT an acceptable production setting
  *) echo "unknown VARIANT=$VARIANT" >&2; exit 2 ;;
esac
exec podman run --rm -i \
  --uidmap 0:0:2000 --uidmap 65534:2000:2 \
  --gidmap 0:0:2000 --gidmap 65534:2000:2 \
  "${extra[@]}" \
  -e VARIANT="$VARIANT" \
  "$IMG" "$@"
