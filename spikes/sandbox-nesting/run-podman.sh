#!/usr/bin/env bash
# Shared podman run invocation with the site uid map (D18) plus optional variants.
# VARIANT is a comma-separated list, e.g. VARIANT=label-disable,unmask (see
# docs/spikes/sandbox-nesting.md). Diagnostic order to try: default, unmask,
# label-disable,unmask, no-seccomp, keep-ns, sys-admin.
#   default        site uid map, default seccomp/SELinux/masks
#   unmask         --security-opt unmask=/proc/*  (podman masks /proc/acpi, /proc/kcore, ... with
#                  bind mounts; a fully-visible procfs is required before a user namespace may
#                  mount a new one, so an inner `--proc /proc` fails with EPERM otherwise)
#   label-disable  --security-opt label=disable   (SELinux-enforcing hosts: the inner proc mount
#                  fails with EACCES under the container_t label)
#   no-seccomp     --security-opt seccomp=unconfined
#   keep-ns        --userns=keep-id INSTEAD of the site uid map (the two are mutually exclusive)
#   sys-admin      --cap-add=SYS_ADMIN; diagnostic only, NOT an acceptable production setting
set -euo pipefail
IMG=${IMG:-localhost/grist-bwrap-spike:latest}
VARIANT=${VARIANT:-default}
extra=()
usermap=(--uidmap 0:0:2000 --uidmap 65534:2000:2 --gidmap 0:0:2000 --gidmap 65534:2000:2)
IFS=, read -r -a variants <<<"$VARIANT"
for v in "${variants[@]}"; do
  case "$v" in
    default)       ;;
    unmask)        extra+=(--security-opt 'unmask=/proc/*') ;;
    label-disable) extra+=(--security-opt label=disable) ;;
    no-seccomp)    extra+=(--security-opt seccomp=unconfined) ;;
    keep-ns)       usermap=(--userns=keep-id) ;;
    sys-admin)     extra+=(--cap-add=SYS_ADMIN) ;;
    *) echo "unknown VARIANT component '$v' in VARIANT=$VARIANT" >&2; exit 2 ;;
  esac
done
exec podman run --rm -i \
  "${usermap[@]}" \
  "${extra[@]}" \
  -e VARIANT="$VARIANT" \
  "$IMG" "$@"
