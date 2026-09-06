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

Every script appends to `results/<hostname>-<date>.log` and prints PASS/FAIL per
step. Paste the log into `docs/spikes/sandbox-nesting.md` and fill in the
outcome table. Do not fix failures silently: record exactly what broke (uid map,
seccomp profile, `--userns` mode, `/proc` mounts), then try the documented
variants in `30-nested.sh` (`VARIANT=` env var).

Nothing here can run in the CI container (no Podman, no bwrap, no user
namespaces there); that is why this milestone is marked 🧑.
