#!/usr/bin/env bash
# Record everything about the node that bears on user-namespace nesting.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd); mkdir -p "$here/results"
log="$here/results/$(hostname -s)-$(date +%Y%m%d).log"
exec > >(tee -a "$log") 2>&1
echo "=== 00-probe $(date -Is) on $(hostname) ==="
echo "--- kernel"; uname -a; cat /etc/os-release 2>/dev/null | head -3
echo "--- podman"; podman --version 2>&1; podman info --format '{{.Host.Security.Rootless}} rootless; runtime={{.Host.OCIRuntime.Name}} {{.Host.OCIRuntime.Version}}; seccomp={{.Host.Security.SECCOMPEnabled}} profile={{.Host.Security.SECCOMPProfilePath}}; apparmor={{.Host.Security.AppArmorEnabled}}; selinux={{.Host.Security.SELinuxEnabled}}' 2>&1
echo "--- bwrap"; bwrap --version 2>&1; command -v bwrap
echo "--- user namespaces"
for f in /proc/sys/user/max_user_namespaces /proc/sys/kernel/unprivileged_userns_clone /proc/sys/kernel/apparmor_restrict_unprivileged_userns; do
  [ -e "$f" ] && echo "$f = $(cat "$f")" || echo "$f absent"
done
echo "--- subuid/subgid for $(id -un)"; grep "^$(id -un):" /etc/subuid /etc/subgid 2>&1 || echo "no subuid/subgid entries (expected under the site's 2002-uid scheme)"
echo "--- id"; id
echo "--- unshare test (unprivileged userns from the shell)"
if unshare -U -r true 2>/dev/null; then echo "PASS unshare -U -r works"; else echo "FAIL unshare -U -r: $(unshare -U -r true 2>&1)"; fi
echo "--- bwrap userns test on the host"
if bwrap --unshare-user --uid 0 --gid 0 --ro-bind / / --proc /proc --dev /dev true 2>/dev/null; then echo "PASS host bwrap --unshare-user"; else echo "FAIL host bwrap --unshare-user: $(bwrap --unshare-user --ro-bind / / true 2>&1)"; fi
echo "=== end 00-probe ==="
