# P0.1 — Sandbox nesting spike (HPC login node)

Risk: *bwrap won't nest in rootless Podman under the site's constrained uid map*
(dev plan §8, §14; decision D18).

Target stack: **rootless Podman → outer bwrap → inner bwrap**, under exactly the
site's flags:

```
podman run --uidmap 0:0:2000 --uidmap 65534:2000:2 ...
podman build --userns-uid-map=0:0:1 --userns-uid-map=1:1:1999 --userns-uid-map=65534:2000:2 ...
```

Fallback: bwrap directly on the login node with no container.

## What a human runs (🧑)

Copy this directory to the login node and run, in order:

```
./00-probe.sh              # versions + userns/seccomp facts; no side effects
./10-build.sh              # builds the bwrap image with the site userns map
./20-outer.sh              # podman run + outer bwrap around a trivial process
./30-nested.sh             # podman run + outer bwrap + inner bwrap (the real test)
./40-fallback-host.sh      # bwrap → bwrap directly on the node, no Podman
```

Every script appends to `results/<hostname>-<date>.log` (git-ignored) and prints
PASS/FAIL per step. Paste the log into `docs/spikes/sandbox-nesting.md` and fill
in the outcome table. Do not fix failures silently: record exactly what broke
(uid map, seccomp profile, `--userns` mode, `/proc` mounts), then try the
documented variants in `30-nested.sh` (`VARIANT=` env var, comma-separated; the
menu is at the top of `run-podman.sh`), in this order: `unmask`,
`label-disable,unmask`, `no-seccomp`, `keep-ns`, `sys-admin`.

Nothing here can run in the CI container (no Podman, no bwrap, no user
namespaces there); that is why this milestone is marked 🧑.

## Rehearsal (2026-09-06, not the login node)

The scripts were dry-run once under a local rootless Podman machine (Podman
6.1.1, Fedora CoreOS VM, kernel 7.1.8, crun, SELinux enforcing, bwrap 0.8.0
in the image) purely to debug them before the real run. That host is not the
HPC site and its results settle nothing about P0.1 or ADR-0002, so they are
deliberately not recorded in the write-up. What the rehearsal fixed:

- `date -Is` is GNU-only; the timestamps now use `date -u +%Y-%m-%dT%H:%M:%SZ`.
- `VARIANT=keep-ns` could never run: `--userns=keep-id` and `--uidmap` are
  mutually exclusive in Podman. It now replaces the site map instead of adding
  to it, which makes it the diagnostic it was meant to be.
- Two variants that the rehearsal host needed were missing: `unmask`
  (Podman's masked paths under `/proc` make the inner `--proc /proc` fail with
  EPERM, because a user namespace may only mount a new procfs when an existing
  one is fully visible) and `label-disable` (on an SELinux-enforcing host the
  same mount fails with EACCES first). `VARIANT` is now a comma-separated list.
- The inner battery's "pid ns isolated" check counted `/proc` entries from a
  command substitution, whose own forks pushed the count past the threshold;
  it now checks the shell's pid, as the `sandbox` crate's test does.
- `results/` and `work/` are git-ignored so a run inside the checkout leaves
  nothing to commit by accident.
