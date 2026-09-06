# Spike P0.1 — Sandbox nesting on the HPC login node

- **Status:** scripts ready; **awaiting a human run on the login node (🧑)**
- **Rehearsal:** the five scripts were dry-run on 2026-09-06 under a local rootless Podman machine (macOS host, Fedora VM, not the login node) purely to debug them; see the spike README. Nothing from that run belongs in the tables below.
- **Milestone:** P0.1 → ADR-0002
- **Scripts:** `spikes/sandbox-nesting/` (see its README for the run order)
- **Risk addressed:** bwrap won't nest in rootless Podman under the site's constrained uid map (dev plan §8, §14; D18)

## What is being tested

The production stack per D18 is **rootless Podman → outer bwrap (trust boundary, owned by the launcher) → inner bwrap (per tool call)**, under exactly:

```
podman build --userns-uid-map=0:0:1 --userns-uid-map=1:1:1999 --userns-uid-map=65534:2000:2 ...
podman run   --uidmap 0:0:2000 --uidmap 65534:2000:2 ...
```

The inner bwrap shape is the one the `sandbox` crate will derive in P1.7: repo RW, everything else RO, tmpfs scratch, network off, scrubbed environment, timeout, own pid namespace.

Why it might fail: nesting needs a second and third unprivileged user namespace from inside a container whose uid map has only 2002 entries and no `subuid` range; Podman's default seccomp profile may block `unshare`/`clone` with `CLONE_NEWUSER`; `/proc` mount restrictions inside the container can stop bwrap's `--proc`; a setuid `bwrap` binary cannot be used rootless.

## Environment (fill in from `00-probe.sh`)

| Item | Value |
|---|---|
| Login node hostname / date | |
| Kernel (`uname -r`) | |
| Distro | |
| Podman version / OCI runtime | |
| bwrap version (host) / (in image) | |
| `/proc/sys/user/max_user_namespaces` (host) / (in container) | |
| `unshare -U -r true` on host | PASS / FAIL |
| Host `bwrap --unshare-user` | PASS / FAIL |
| seccomp enabled / profile path | |
| AppArmor / SELinux | |

## Results (fill in; paste the relevant log lines under each)

| Step | Script | Variant | Outcome | Notes (what broke) |
|---|---|---|---|---|
| Image build with site userns map | `10-build.sh` | – | | |
| Podman → outer bwrap, trivial process | `20-outer.sh` | default | | |
| Podman → outer → inner battery | `30-nested.sh` | default | | |
| Podman → outer → inner battery | `30-nested.sh` | `unmask` | | only if default failed: `--security-opt unmask=/proc/*` (the inner `--proc` needs a fully visible procfs) |
| Podman → outer → inner battery | `30-nested.sh` | `label-disable,unmask` | | only if `unmask` failed with EACCES on an SELinux-enforcing host |
| Podman → outer → inner battery | `30-nested.sh` | `no-seccomp` | | only if the above failed |
| Podman → outer → inner battery | `30-nested.sh` | `keep-ns` | | drops the site uid map for `--userns=keep-id` (mutually exclusive); a diagnostic, not the target |
| Podman → outer → inner battery | `30-nested.sh` | `sys-admin` | | diagnostic only, never a production setting |
| Host bwrap → bwrap (fallback, no container) | `40-fallback-host.sh` | – | | |

Inner battery checks (each must PASS for a variant to count as green): trivial process, repo RW, root RO, scratch tmpfs, **network off**, env scrubbed, timeout enforced, pid namespace isolated.

## Precisely what broke (if anything)

Record per item, with the exact error text:

- 2002-uid map (`--uidmap 0:0:2000 --uidmap 65534:2000:2`):
- Podman default seccomp profile:
- `--userns` mode:
- `/proc` mounts inside nested namespaces (Podman's masked paths under `/proc`; SELinux label on an enforcing host):
- Other:

## Fallbacks (only if neither the nested stack nor host bwrap works)

Per dev plan §14: namespace-per-container (one Podman container per tool call, no inner bwrap) and gVisor. Record what was evaluated and the result here; the decision goes in ADR-0002.

## Outcome → ADR-0002

- [ ] Nested stack green under the site map with `VARIANT=default` → ADR-0002 adopts it as-is
- [ ] Green only with a variant → ADR-0002 records the required Podman flags and whether the site permits them
- [ ] Only the host-bwrap fallback is green → ADR-0002 adopts bwrap-on-login-node for the kernel, and the container becomes a packaging concern
- [ ] Nothing green → ADR-0002 evaluates namespace-per-container vs. gVisor

## Reproduce

```
scp -r spikes/sandbox-nesting <login-node>:~/ && ssh <login-node>
cd sandbox-nesting && ./00-probe.sh && ./10-build.sh && ./20-outer.sh && ./30-nested.sh && ./40-fallback-host.sh
# on failure of 30, in this order, recording each:
#   VARIANT=unmask ./30-nested.sh ; VARIANT=label-disable,unmask ./30-nested.sh
#   VARIANT=no-seccomp ./30-nested.sh ; VARIANT=keep-ns ./30-nested.sh
cat results/*.log
```
