# Addendum: OpenShift deployment

| | |
|---|---|
| **Status** | proposed (2026-10-04); not scheduled until accepted |
| **Amends** | the Backlog rows "OpenShift placement for the kernel" and "Websocket transport", P3.3 placement; D11 and D18 when adopted (an ADR, numbered at acceptance, records it) |
| **Does not change** | the kernel, the event log, profiles, the capability model, or the login-node plan (bwrap stays the sandbox there) |

## 1. Position

On OpenShift, **the pod is the sandbox, not bwrap.** On the login node we are one unprivileged user, so isolation has to come from user namespaces (bwrap). On OpenShift the platform already isolates pods: a random UID, no capabilities, seccomp, and network policy, all from the default `restricted-v2` SCC. Running bwrap inside a pod would need a custom SCC that weakens exactly those defaults.

So OpenShift gets a new `SandboxBackend`, `Pod`. It turns the same derived `Policy` into a pod spec, the same way `Bwrap` turns it into argv. Capability atoms, `derive_policy`, monotone narrowing and the event log are untouched.

Our team deploys with a privileged account. That privilege is used **once**, to install the chart, the namespace, RBAC and quotas. Nothing grist runs at runtime is privileged.

## 2. Topology

Three kinds of pod, all `restricted-v2`:

| Pod | Created by | Holds | Count |
|---|---|---|---|
| **orchestrator** (`grist-daemon`) | the team's Helm release (a `Deployment`) | a ServiceAccount allowed to create, watch and delete pods in its own namespace, nothing broader; no model credentials | `replicas: N`, scaled by hand or an HPA |
| **kernel** (`grist-kernel`, one session) | the orchestrator, from the chart's kernel pod template | the model-endpoint secret (D10: only the provider client reads it); **no** ServiceAccount token | one per running session |
| **tool** (`grist-toolhost`, one session) | the orchestrator, from the chart's tool pod template, with mounts and egress from the session's `Policy` | the session's workdir mount; **no** secrets, **no** token | one per running session |

Why the kernel and tool pods are separate:
- A tool in the kernel's pod could read the kernel's environment through `/proc` as the same UID, breaking D10.
- It would also inherit the kernel's egress to the model endpoint, breaking "no `net` atom, no network".

Separate pods give each one its own network policy and no shared process view. Two pods per session cost a few seconds of start-up, which is acceptable for sessions measured in minutes.

**"Scale from one deployment spec"** works at two levels, and both come from the one Helm release:
- The orchestrator is an ordinary `Deployment`. Raise `replicas`, or attach an HPA, to add capacity.
- Every kernel and tool pod is stamped from pod templates in the chart's values. Changing the image, resources or runtime class there changes every new session.
- Agent definitions (the `profiles/` tree and `catalog.toml`) ship as a ConfigMap, so adding an agent is a values change, not an image rebuild.

## 3. How the pieces map

| Policy (P1.7) | bwrap (login node) | `Pod` backend (OpenShift) |
|---|---|---|
| `fs.rw` / `fs.ro` | `--bind` / `--ro-bind` | `volumeMounts` on the session PVC, `readOnly` per atom; `readOnlyRootFilesystem: true` |
| tmpfs scratch | `--tmpfs` | `emptyDir { medium: Memory, sizeLimit }` |
| network off / `net:` allowlist | `--unshare-net` | a per-session `NetworkPolicy`: deny all egress, then allow the listed destinations (see open question 1) |
| timeout | deadline, then SIGTERM/SIGKILL | per call, enforced by `grist-toolhost` the same way; `activeDeadlineSeconds` caps the pod |
| scrubbed env (D10) | `--clearenv` + allowlist | `env` written out explicitly; `automountServiceAccountToken: false`; `enableServiceLinks: false` |
| `Session` tool (REPL, MCP) | one bwrap process per session | a process in the session's tool pod, launched by `grist-toolhost` |
| `Stateless` call (`bash`) | a fresh bwrap per call | a fresh process in the tool pod: fresh process state, but **not** a fresh filesystem view. Stronger option, off by default: a `Job` per call (seconds each) |
| stronger isolation | n/a | optional `runtimeClassName: kata` (OpenShift sandboxed containers) on tool pods, a chart value |

**Kernel ↔ tool pod.** `grist-toolhost` serves the existing ADR-0001 JSON-RPC launcher protocol over TCP instead of stdio, authenticated by a per-session token the orchestrator puts in both pods. A NetworkPolicy admits ingress to a tool pod only from its own kernel pod. This replaces the stdio pipe, not the protocol.

**State.** Event logs, checkpoints, artifacts and notebooks go on a `ReadWriteMany` PVC (or object storage later, behind `ArtifactStore`). Because resume is restore-from-checkpoint (`Kernel::open`), **any orchestrator replica can resume any suspended session.** A session that suspends on a long task releases both pods, and resumes in fresh ones when the task completes. A Kubernetes `Lease` per session guarantees one writer per log, which NFS file locks do not reliably give.

**Models.** The kernel pod reaches vLLM (in-cluster, e.g. OpenShift AI / KServe) or LiteLLM by Service DNS. The cluster's trusted CA bundle is injected into a ConfigMap (`config.openshift.io/inject-trusted-cabundle: "true"`), mounted read-only, and named by `GRIST_CA_BUNDLE`, which the HTTP client already honours.

## 4. How a person connects

**v1 needs no new transport.** The orchestrator listens on TCP bound to `127.0.0.1` inside its pod, next to the unix socket. A developer runs:

```sh
oc port-forward deploy/grist-daemon 7411:7411      # authenticated by OpenShift RBAC (pods/portforward)
# Zed agent_servers args: ["--tcp", "127.0.0.1:7411"]   (grist-connect, as in docs/clients/zed.md §2)
```

Binding to loopback means the port is unreachable from other pods, so OpenShift's own authentication replaces the peer-UID check (D11) as the security boundary. With several replicas, `oc port-forward` picks one, and sessions are listed from the shared PVC so any replica can serve a `session/load`.

**v2** is the Backlog's websocket transport: a `Route` with TLS and an OAuth proxy in front of the orchestrator, for users without `oc` access. It is scheduled only when that need appears.

## 5. Milestones (a parallel track, after P2.4)

| # | Milestone | Done when |
|---|---|---|
| OS.0 | Helm chart: images for daemon, kernel and toolhost; namespace, RBAC, quota, default-deny NetworkPolicy; profiles as a ConfigMap; single replica, kernel processes inside the orchestrator pod (dev only, logged like the `None` backend) | 🧑 a session runs on the team's cluster through `oc port-forward` and Zed |
| OS.1 | `Pod` sandbox backend + `grist-toolhost`; per-session tool pod and NetworkPolicy from `Policy` | the P1.7 sandbox tests pass against a kind/CRC cluster in an opt-in CI job, like `ci:standin` |
| OS.2 | Kernel pod per session; RWX PVC; per-session `Lease`; suspend releases pods, resume on any replica | kill an orchestrator replica mid-session; the session resumes on another with `diff-logs` identical |
| OS.3 | Scaling: `replicas` and HPA on active sessions; per-session resource requests from the agent profile; optional Kata runtime class | 🧑 N concurrent sessions across M replicas within quota |
| OS.4 | Multi-agent: P2.4 `spawn` maps to the orchestrator launching child kernel and tool pods, narrowed per D6 | the P2 exit test (model debugger cannot reach meshing tools) passes on the cluster |

OS.0 can start any time after this addendum is accepted. It needs only P1.9, which has landed.

## 6. Open questions (for the security team and us)

1. **Egress by hostname.** `net:` atoms name hosts, but `NetworkPolicy` matches IPs and pods. Options: resolve hosts at launch (fragile), use OVN-Kubernetes `EgressFirewall` with `dnsName` (namespace-wide, not per session), or put a per-namespace HTTP egress proxy with a host allowlist in front. The proxy is the likely answer.
2. **Slurm from OpenShift.** Can pods submit to the HPC cluster, and can a Slurm epilog reach a waker in the cluster? If not, OpenShift sessions are limited to in-cluster work, and HPC work stays on the login node (D18).
3. **Workdir source.** Is the RWX PVC populated by `git clone` at session start, or is it a shared project filesystem? This decides whether login-node and cluster sessions can share files.
4. **Kata availability** on the target cluster, and whether the security team requires it for tool pods.
5. Whether a custom resource (`kind: GristAgent`) is worth adding later, or whether the profiles ConfigMap stays enough. See the kagent notes in §7.

## 7. Related work: kagent

*Pending: the research write-up is being added in a follow-up section.*
