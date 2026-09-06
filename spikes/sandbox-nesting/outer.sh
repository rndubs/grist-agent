#!/usr/bin/env bash
# Outer bwrap: the trust boundary around the whole kernel process (dev plan §8).
# Runs INSIDE the container. Wraps "$@" with: root RO, /work RW, private /tmp,
# own pid namespace, new user namespace (needed so the inner bwrap can unshare again).
set -euo pipefail
exec bwrap \
  --unshare-user --unshare-pid --unshare-ipc --unshare-uts \
  --uid 1000 --gid 1000 \
  --ro-bind / / \
  --bind /work /work \
  --tmpfs /tmp \
  --proc /proc --dev /dev \
  --die-with-parent --new-session \
  -- "$@"
