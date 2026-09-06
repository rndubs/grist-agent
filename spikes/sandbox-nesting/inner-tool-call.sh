#!/usr/bin/env bash
# Inner bwrap: the per-tool-call shape from the plan (P0.1):
#   repo RW, everything else RO, tmpfs scratch, network OFF, timeout.
# Runs INSIDE the outer bwrap. Executes "$@" and prints checks.
set -uo pipefail
TIMEOUT=${TIMEOUT:-10}
run_inner() {
  timeout --signal=TERM "$TIMEOUT" bwrap \
    --unshare-user --unshare-pid --unshare-net --unshare-ipc --unshare-uts \
    --uid 1000 --gid 1000 \
    --ro-bind / / \
    --bind /work /work \
    --tmpfs /scratch --tmpfs /tmp \
    --proc /proc --dev /dev \
    --clearenv --setenv PATH /usr/local/bin:/usr/bin:/bin --setenv HOME /scratch \
    --die-with-parent --new-session \
    -- "$@"
}
if [ "$#" -gt 0 ]; then run_inner "$@"; exit $?; fi

# Self-test battery when called with no args.
rc=0
expect_ok()   { local name=$1; shift; if   "$@" ; then echo "PASS inner: $name"; else echo "FAIL inner: $name"; rc=1; fi; }
expect_fail() { local name=$1; shift; if ! "$@" ; then echo "PASS inner: $name"; else echo "FAIL inner: $name"; rc=1; fi; }
expect_ok   "trivial process"               run_inner true
expect_ok   "repo RW"                       run_inner sh -c 'echo hi > /work/.spike-write && rm /work/.spike-write'
expect_fail "root RO (write to /usr fails)" run_inner sh -c 'touch /usr/spike 2>/dev/null'
expect_ok   "scratch tmpfs writable"        run_inner sh -c 'echo x > /scratch/x'
expect_fail "network off"                   run_inner python3 -c 'import socket; s=socket.socket(); s.settimeout(2); s.connect(("1.1.1.1",53))'
SECRET=leak expect_ok "env scrubbed (no SECRET)" run_inner sh -c '[ -z "${SECRET:-}" ]'
TIMEOUT=2   expect_fail "timeout enforced"  run_inner sleep 30
expect_ok   "pid ns isolated"               run_inner sh -c '[ "$(ls /proc | grep -c "^[0-9]")" -le 3 ]'
exit $rc
