# CI stand-in stack (P0.5)

Everything agents need to run integration tests without GPUs, cluster access, or
in-house tools (decisions D9, D18). Four pieces, all under `standin/`:

| Piece | What it stands in for | Where |
|---|---|---|
| llama.cpp server | vLLM: an OpenAI-compatible `/v1/chat/completions` with tool calling and SSE | `standin/compose.yaml` → service `llama` |
| LiteLLM proxy | the site LiteLLM: `provider/model` routing behind a master key | service `litellm`, `standin/litellm/litellm_config.yaml` |
| Fake Slurm | `sbatch` / `squeue` / `scancel` / `sacct` on the HPC login node, incl. the epilog hook | `standin/slurm/` |
| Mock in-house solver | a pre-installed simulation code at a fixed path, submitted via Slurm | `standin/solver/opt/acme-solver/` |

The fake Slurm and the mock solver are plain scripts (bash / Python 3.11 stdlib)
and run anywhere; the two model services need Docker or Podman.

## Pins

| Component | Pin |
|---|---|
| llama.cpp server image | `ghcr.io/ggml-org/llama.cpp:server-b10818` (multi-arch: amd64, arm64) |
| Model | `Qwen/Qwen2.5-1.5B-Instruct-GGUF` / `qwen2.5-1.5b-instruct-q4_k_m.gguf` (~1.1 GB, Q4_K_M), downloaded at first start into `standin/.cache/models` (git-ignored); never committed |
| LiteLLM image | `ghcr.io/berriai/litellm:v1.99.1` |
| "login node" base image | `python:3.11-slim-bookworm` |
| Fake Slurm reports | `slurm 23.11.4-fake` (the `-fake` suffix is how tooling tells it from the real thing) |
| Mock solver reports | `ACME Solver 7.4.2 (standin)` |

Bump the image tags and the model pin together and re-check the
"expected behaviour" notes below; `standin/smoke.sh` is the acceptance test.

## Quick start (local)

```sh
standin/up.sh                    # writes standin/.env with a random LITELLM_MASTER_KEY,
                                 # pulls images, downloads the GGUF once, waits for health
eval "$(standin/up.sh --print-env)"   # STANDIN_OPENAI_BASE_URL, STANDIN_LITELLM_BASE_URL,
                                      # STANDIN_LITELLM_KEY, STANDIN_MODEL
standin/smoke.sh                 # (a) tool call via LiteLLM, (b) SSE via llama.cpp,
                                 # (c) sbatch -> mock solver -> epilog, inside the slurm service
standin/env-gate.sh              # which test tiers this shell can reach
standin/up.sh --down             # stop; the model cache is kept
```

Without Docker you can still use the fake Slurm and the solver directly:

```sh
export PATH="$PWD/standin/slurm/bin:$PWD/standin/solver/opt/acme-solver/bin:$PATH"
standin/slurm/test.sh && standin/solver/test.sh          # self-tests
STANDIN_SLURM_EXEC=local standin/smoke.sh slurm          # smoke part (c) on the host
```

Podman: `COMPOSE="podman compose" standin/up.sh` (or `podman-compose`). The
compose file uses only `image`, `command`, `environment`, `volumes`, `ports`,
`healthcheck`, `depends_on: condition: service_healthy`, `init`, so it works
with both engines. Under rootless Podman on the login node use the D18 uidmap
flags; the stand-in itself needs no privileges.

## Ports and variables

Host ports (compose interpolation variables, put them in `standin/.env`):

| Variable | Default | Meaning |
|---|---|---|
| `LITELLM_MASTER_KEY` | **required** (`up.sh` generates one) | LiteLLM master key; clients send `Authorization: Bearer <key>` |
| `STANDIN_LLAMA_PORT` | `8080` | host port for llama.cpp (`/v1/*`, `/health`, `/metrics`, `/props`, `/slots`) |
| `STANDIN_LITELLM_PORT` | `4000` | host port for LiteLLM (`/v1/*`, `/health/readiness`, `/health/liveliness`) |
| `STANDIN_MODEL` | `qwen2.5-1.5b-instruct` | llama.cpp `--alias`; the LiteLLM route is `stand-in/<this>` and must match `litellm_config.yaml` |
| `STANDIN_HF_REPO` / `STANDIN_HF_FILE` | the pinned Qwen2.5 GGUF | what llama.cpp downloads (`--hf-repo` / `--hf-file`) |
| `STANDIN_MODEL_CACHE` | `./.cache/models` | bind mount for `LLAMA_CACHE`; CI caches this directory |
| `STANDIN_LLAMA_THREADS` | `-1` (auto) | `--threads` for llama.cpp |
| `STANDIN_LITELLM_LOG` | `INFO` | LiteLLM log level |
| `STANDIN_SLURM_EPILOG` | `/opt/fake-slurm/libexec/default-epilog` | `FAKE_SLURM_EPILOG` inside the `slurm` service |
| `HF_TOKEN` | empty | only needed for gated Hugging Face repos |

Host-side environment for clients and tests (printed by `standin/up.sh --print-env`,
exported by the CI job; the P1.5 integration tests read exactly these):

| Variable | Value in the stack |
|---|---|
| `STANDIN_OPENAI_BASE_URL` | `http://127.0.0.1:8080/v1` (llama.cpp, no auth) |
| `STANDIN_LITELLM_BASE_URL` | `http://127.0.0.1:4000/v1` |
| `STANDIN_LITELLM_KEY` | the master key |
| `STANDIN_MODEL` | `qwen2.5-1.5b-instruct`; address it as `stand-in/$STANDIN_MODEL` through LiteLLM and as `$STANDIN_MODEL` directly |

Inside the `slurm` service the same four are set with the compose-network
URLs (`http://llama:8080/v1`, `http://litellm:4000/v1`).

## llama.cpp endpoint: expected behaviour

Server flags (see `compose.yaml`): `--jinja` (chat-template tool calling; without it
no `tool_calls` are ever produced), `--ctx-size 8192 --parallel 2` (4096 tokens per
slot), `--n-predict 1024`, `--alias`, `--no-webui`, `--metrics`.

Qwen2.5-Instruct's embedded chat template declares tools in the system prompt and
the model answers with Hermes-style `<tool_call>{"name":..,"arguments":{..}}</tool_call>`
blocks; with `--jinja` llama.cpp parses them into the OpenAI shape:

- `choices[0].message.tool_calls[]` = `{ "id": "<random>", "type": "function", "function": { "name", "arguments": "<JSON string>" } }`, `finish_reason: "tool_calls"`, `content` null or empty.
- `tool_choice: "auto"` (default) and `"required"` are honoured; `"required"` and named
  tool choices are enforced with a grammar. `"none"` disables tools.
- `parallel_tool_calls` is supported; the 1.5B model rarely emits more than one call.
- Streaming (`stream: true`): SSE `data: {...}` chunks, `delta.role` on the first,
  `delta.content` pieces, a final chunk with `finish_reason`, `usage` in the last chunk
  when `stream_options.include_usage` is set, then `data: [DONE]`. Tool-call deltas
  stream as `delta.tool_calls[{index, id, function: {name, arguments}}]` fragments.
- `response_format: {"type": "json_object"}` and `{"type": "json_schema", ...}` are
  implemented with grammars, so the P0.2 structured-output probe should report
  `supported` here (record it as a quirk flag; vLLM/LiteLLM upstreams differ).
- Not supported: `n > 1`, `logit_bias` by token string, `user`, most OpenAI-only
  fields (ignored). Unknown fields are ignored, not rejected.
- No authentication; `Authorization` headers are ignored. `/v1/models` lists the alias.

**`reasoning_content`:** Qwen2.5-Instruct is not a reasoning model and emits no
`<think>` blocks, so `message.reasoning_content` is **absent** in both non-streaming
and streaming responses from the default stand-in. llama.cpp's `--reasoning-format`
defaults to `auto`, which only separates reasoning for templates that produce it.
To exercise the reasoning path (P0.2, P1.5 `Thinking` block mapping) swap the model
without touching anything else, e.g. in `standin/.env`:

```
STANDIN_HF_REPO=Qwen/Qwen3-1.7B-GGUF
STANDIN_HF_FILE=Qwen3-1.7B-Q4_K_M.gguf
STANDIN_MODEL=qwen3-1.7b
```

and add a matching `model_name: stand-in/qwen3-1.7b` entry to
`litellm_config.yaml`. Qwen3 emits `<think>…</think>` which llama.cpp returns as
`reasoning_content` (streamed as `delta.reasoning_content`); thinking can be turned
off per request with `"chat_template_kwargs": {"enable_thinking": false}`. Tool
calling still works but a thinking 1.7B model on CPU is slow (tens of seconds per
call) and `smoke.sh` is tuned for the Qwen2.5 default, so keep Qwen2.5 as the CI pin
and use Qwen3 for human-run probes. Whatever P0.2 measures against this endpoint
goes into the quirk-flag table in `docs/spikes/providers.md`.

Performance on a 4-vCPU GitHub runner: ~10–20 generated tokens/s, a tool call
returns in a few seconds after a first-request warm-up of a few seconds.
`temperature: 0` plus `seed` gives repeatable output for a given build.

## LiteLLM proxy

`standin/litellm/litellm_config.yaml` maps `stand-in/qwen2.5-1.5b-instruct` to
`openai/qwen2.5-1.5b-instruct` with `api_base: http://llama:8080/v1`. Notes:

- Master key comes from `os.environ/LITELLM_MASTER_KEY`; nothing is hardcoded. There
  is no database, so the master key is the only key (no virtual keys, no spend log).
- `drop_params: true`: unknown request params are dropped rather than rejected.
- `GET /v1/models` (with the key) lists `stand-in/qwen2.5-1.5b-instruct`.
- Tool calls pass through unchanged; `reasoning_content` from an OpenAI-compatible
  upstream is passed through on `message.reasoning_content` in current LiteLLM
  versions. P0.2 records what actually arrives.
- Adding a model: add a `model_list` entry; the `stand-in/` prefix is a naming
  convention, not a wildcard.

## Fake Slurm

`standin/slurm/bin/{sbatch,squeue,scancel,sacct}` (bash), a shared library in
`lib/fake-slurm.sh`, and the detached job runner `libexec/fake-slurm-runner`.
Put `standin/slurm/bin` on `PATH`; nothing else is needed.

### Lifecycle

`sbatch` allocates an id (from 1001), copies the script, parses `#SBATCH`
directives, prints `Submitted batch job <id>` (or just the id with `--parsable`),
and detaches a runner with `setsid nohup`. The runner sets the state to `RUNNING`,
executes the script in its own process group with the `SLURM_*` environment,
appends stdout/stderr to the job's output file, records the exit code, sets the
final state, appends Slurm's `*** JOB n ON node CANCELLED AT … ***` trailer for
cancelled/timed-out jobs, then runs the epilog.

States: `PENDING` → `RUNNING` → `COMPLETED` | `FAILED` | `CANCELLED` | `TIMEOUT`.
`squeue` shows `PD R CD F CA TO` and, like Slurm, keeps finished jobs visible for
`FAKE_SLURM_MIN_JOB_AGE` seconds (default 300); `squeue -t all` and `sacct` show
everything the state directory knows.

### Supported options

- `sbatch`: `-J/--job-name`, `-o/--output`, `-e/--error` (patterns `%j %x %u %N %A %%`),
  `-D/--chdir`, `-p/--partition`, `-t/--time` (all Slurm forms; enforced with
  `timeout` → state `TIMEOUT`), `-N -n -c`, `--mem`, `-A`, `--export=ALL|NONE|VAR=v,…`,
  `--wrap`, `--parsable`, `--wait`/`-W` (exits with the job's exit code), `--version`,
  script on stdin, script arguments. Other value-taking options are accepted and
  ignored; unknown bare options are rejected like real `sbatch`. `--array` and
  `--hold` are rejected (unsupported).
- `squeue`: `-j`, `-u`, `--me`, `-n`, `-p`, `-t STATES|all`, `-h`, `-o FORMAT` with
  `%i %j %u %P %a %t %T %M %l %D %C %N %R %Z %o %S %e %V %A` and Slurm width specs
  (`%.18i` right-, `%8j` left-justified), `-l`.
- `scancel`: job ids, `-n`, `-u`, `--me`, `-s SIGNAL` (signal only, no state change),
  `-q`, `-v`. Cancel = SIGTERM to the script's process group, SIGKILL after
  `FAKE_SLURM_KILL_WAIT` seconds (default 10; Slurm's KillWait is 30). Cancelling a
  `PENDING` job marks it `CANCELLED` without running it or the epilog (Slurm behaves
  the same: the epilog runs only for jobs that ran). Cancelling a finished or unknown
  job is an error, as in Slurm.
- `sacct`: `-j`, `-u`, `-s`, `-n`, `-P`/`-p`, `-X`, `-o/--format` with `JobID JobName
  Partition Account AllocCPUS State ExitCode DerivedExitCode Start End Submit Elapsed
  Timelimit User UID NodeList NNodes WorkDir Cluster`. `ExitCode` is `rc:signal`
  (`0:15` for a TERM-killed job, `7:0` for `exit 7`); a cancelled job's `State` is
  `CANCELLED by <uid>`.

### Environment inside the job

`SLURM_JOB_ID`, `SLURM_JOBID`, `SLURM_JOB_NAME`, `SLURM_JOB_USER`, `SLURM_JOB_PARTITION`,
`SLURM_JOB_ACCOUNT`, `SLURM_SUBMIT_DIR`, `SLURM_SUBMIT_HOST`, `SLURM_JOB_NODELIST`
(`fakenode01`), `SLURM_NODELIST`, `SLURMD_NODENAME`, `SLURM_JOB_NUM_NODES`, `SLURM_NNODES`,
`SLURM_NTASKS`, `SLURM_NPROCS`, `SLURM_CPUS_PER_TASK`, `SLURM_CPUS_ON_NODE`,
`SLURM_JOB_CPUS_PER_NODE`, `SLURM_TASKS_PER_NODE`, `SLURM_CLUSTER_NAME` (`fake`),
`SLURM_JOB_QOS`, `SLURM_PROCID`, `SLURM_NODEID`, `SLURM_LOCALID`, `SLURM_JOB_STDOUT`,
`SLURM_JOB_STDERR`, plus `FAKE_SLURM_STATE_DIR`. The job otherwise inherits the
submitter's environment (`--export=ALL`), or a scrubbed one with `--export=NONE`.

### Epilog hook contract (this is what the P3.4 waker consumes)

If `FAKE_SLURM_EPILOG` is set **at submission time** to an executable, the runner
invokes it once, after the final state is recorded, for every job that actually
ran (also for `FAILED`, `CANCELLED`, `TIMEOUT`). It runs in the job's work dir with
a 60 s timeout; its output goes to `<state>/jobs/<id>/epilog.log` and its exit
status to `epilog.rc`. Environment:

| Variable | Value |
|---|---|
| `SLURM_JOB_ID`, `SLURM_JOBID` | job id |
| `SLURM_JOB_NAME`, `SLURM_JOB_USER`, `SLURM_JOB_UID`, `SLURM_JOB_PARTITION`, `SLURM_JOB_NODELIST`, `SLURM_JOB_WORK_DIR` | as submitted |
| `SLURM_JOB_EXIT_CODE` | raw exit status (`0`, `7`, `143` for SIGTERM, `137` for SIGKILL) |
| `SLURM_JOB_EXIT_CODE2` | Slurm's `exit:signal` form (`7:0`, `0:15`) |
| `SLURM_JOB_DERIVED_EC` | same as `SLURM_JOB_EXIT_CODE` |
| `SLURM_SCRIPT_CONTEXT` | `epilog_slurmctld` |
| `SLURM_JOB_STDOUT`, `SLURM_JOB_STDERR` | resolved output paths |
| `FAKE_SLURM_JOB_STATE` | `COMPLETED` / `FAILED` / `CANCELLED` / `TIMEOUT` (fake-only convenience; real Slurm epilogs derive it from the exit code) |
| `FAKE_SLURM_JOB_DIR`, `FAKE_SLURM_STATE_DIR` | where the job's files live |

`libexec/default-epilog` appends one line per job to `$FAKE_SLURM_STATE_DIR/epilog.log`
and is the default inside the compose `slurm` service.

### Knobs

| Variable | Default | Effect |
|---|---|---|
| `FAKE_SLURM_STATE_DIR` | `$TMPDIR/fake-slurm-<uid>` (or `/tmp/…`) | state directory; use a fresh one per test |
| `FAKE_SLURM_EPILOG` | unset | epilog executable, captured per job at `sbatch` time |
| `FAKE_SLURM_PENDING_SECONDS` | `0` | simulated queue wait before `RUNNING` |
| `FAKE_SLURM_KILL_WAIT` | `10` | seconds between SIGTERM and SIGKILL on `scancel` |
| `FAKE_SLURM_MIN_JOB_AGE` | `300` | how long finished jobs stay in the default `squeue` view |
| `FAKE_SLURM_NODE` | `fakenode01` | node name reported everywhere |
| `FAKE_SLURM_DEFAULT_PARTITION` | `standin` | default partition |

State directory layout is documented at the top of `lib/fake-slurm.sh`. A `RUNNING`
job whose runner died (kill -9, reboot) is reported as `FAILED` on the next query.

### Differences from real Slurm (know these before relying on them)

No scheduler: every job starts immediately (unless `FAKE_SLURM_PENDING_SECONDS`), no
resource accounting, one virtual node, no job arrays, no steps (`srun` does not
exist; `sacct` shows the allocation only), no `--hold`/`--dependency`, `sacct`
without `-j` lists all jobs of the state dir regardless of time window, and the
epilog runs in the submitter's user context rather than as root. `sbatch --version`
prints `slurm 23.11.4-fake` so tooling can tell them apart (`env-gate.sh` does).

`standin/slurm/test.sh` proves: submit → `squeue` `RUNNING` → `COMPLETED` with the
epilog receiving the job id and exit code; exit-code propagation (`FAILED`,
`sacct 7:0`, `--wait` returns 7); `scancel` of a running job (prompt SIGTERM,
`0:15`, process group gone) and of a TERM-ignoring job (SIGKILL escalation, `0:9`);
cancel while `PENDING` (no epilog); time limit → `TIMEOUT`; `--chdir`, `--output`/
`--error`, `--export=NONE`, stdin scripts, `#SBATCH` directives, `%x-%j` patterns;
error handling; `squeue` filters and widths. 55 assertions, ~15 s.

## Mock in-house solver

Install-path convention: the solver lives at **`/opt/acme-solver`** with the binary
at `/opt/acme-solver/bin/solve` and example decks in `/opt/acme-solver/share/decks/`.
In the repo that tree is `standin/solver/opt/acme-solver/`; the compose `slurm`
service mounts it read-only at `/opt/acme-solver` and puts `bin` on `PATH`. This is
the shape the D9 capability bundle will grant (`Fs{/opt/acme-solver, ro}` +
`Proc{sbatch}`, P1.8) and what the P2.5 log-extraction pass develops against.

```
solve [--outdir DIR] [--log FILE] [--step-seconds S] [--version] DECK
```

Deck: `key = value` lines, `#` comments, all keys optional.

| Key | Default | Meaning |
|---|---|---|
| `name` | deck file stem | run name (header, result file) |
| `mesh_cells` | 4000 | cosmetic size (header, fake dof count) |
| `steps` | 10 | outer time steps |
| `step_seconds` | 0.1 | wall time per step (sleep); overridden by `--step-seconds` or `ACME_STEP_SECONDS` |
| `max_iters` | 8 | inner iterations per step |
| `tolerance` | 1e-6 | inner convergence tolerance |
| `initial_residual` | 1.0 | residual at the first inner iteration of each step |
| `convergence_rate` | 0.08 | per-iteration residual multiplier (`0.9` with few `max_iters` gives a stall → exit 4) |
| `fail_at_step` | 0 | `N` > 0: abort with `ACME-E303` when step N starts (exit 3) |
| `fail_message` | `negative Jacobian in element {element} (step {step})` | text after `ACME-E303 fatal:` |
| `diverge` | false | grow residuals instead of shrinking them |
| `diverge_at_step` | 1 | first diverging step |
| `growth_rate` | 12.0 | per-iteration multiplier while diverging |
| `diverge_threshold` | 1e10 | `ACME-E201` fires above this (exit 2) |
| `seed` | 42 | jitter seed; the log is deterministic per deck |
| `result_file` | `acme-result.json` | written into `--outdir` (default cwd) |
| `dt` | 0.01 | cosmetic time step size |

Log shape (parse targets in bold): banner with version/host/pid/start time and the
Slurm job id when present; the echoed deck; `Assembling system … done (t s)`;
per step `--- step N  t = … ---`, per iteration **`  iter K  resid = 1.234e-05   lin.iters = 12`**,
then **`  step N converged in K iterations  (t s)`** or **`ACME-W410 step N did not
converge …`**; a `Summary` block with steps completed, total iterations, final
residual, **wall time**, solve time, result file, error; and the final line
**`Run status: CONVERGED|DIVERGED|ABORTED|NOT_CONVERGED|INTERRUPTED`**.

Exit codes and markers:

| Exit | Status | Marker line |
|---|---|---|
| 0 | `CONVERGED` | — |
| 1 | usage/deck error | `ACME-E100 …` on stderr |
| 2 | `DIVERGED` | `ACME-E201 solution diverged at step N iter K: residual … exceeds …` |
| 3 | `ABORTED` | `ACME-E303 fatal: …`, fake traceback, `*** ABORT ***` |
| 4 | `NOT_CONVERGED` | `ACME-W410 step N did not converge in K iterations` |
| 143 | `INTERRUPTED` | `ACME-W900 received signal 15, writing checkpoint …` (what `scancel` produces) |

The result file (`acme-result.json`) carries `status`, `exit_code`, `steps_completed`,
`total_iterations`, `final_residual`, `wall_seconds`, `slurm_job_id`, deck path,
host, timestamps. Example decks: `converge.deck` (exit 0), `diverge.deck` (exit 2 at
step 3), `crash.deck` (exit 3 at step 4). `standin/solver/test.sh` runs all three
plus the stall, bad-deck, missing-deck, `--log`, determinism and SIGTERM cases
(33 assertions, ~2 s).

Through the fake Slurm (this is smoke part (c), `slurm/libexec/sbatch-solver-check`):

```sh
sbatch --parsable -J acme-converge -o %x-%j.out \
  --wrap "solve /opt/acme-solver/share/decks/converge.deck --outdir run1"
```

## The `slurm` compose service

`python:3.11-slim-bookworm` + `sleep infinity` with `init: true` (reaps the detached
runners). Mounts: `standin/slurm` → `/opt/fake-slurm` (ro), `standin/solver/opt/acme-solver`
→ `/opt/acme-solver` (ro), named volumes `/scratch` (work dir) and
`/var/lib/fake-slurm` (state). `PATH` starts with `/opt/fake-slurm/bin:/opt/acme-solver/bin`.
Healthcheck: `sbatch --version && solve --version && squeue -h`.

```sh
docker compose -f standin/compose.yaml exec slurm bash   # a shell "on the login node"
docker compose -f standin/compose.yaml exec -T slurm /opt/fake-slurm/libexec/sbatch-solver-check
```

## CI job

`.github/workflows/ci.yml` → job `standin` (ubuntu-latest, 40 min cap, independent of
the Rust jobs):

1. `actions/cache@v4` restores `standin/.cache/models` (key from `compose.yaml`'s hash,
   `restore-keys: standin-gguf-`), so the GGUF is downloaded only when the pin changes.
2. Host self-tests: `standin/slurm/test.sh`, `standin/solver/test.sh`.
3. `standin/up.sh` with a per-run random `LITELLM_MASTER_KEY`; waits for all three
   healthchecks (`--wait`).
4. Exports `STANDIN_OPENAI_BASE_URL`, `STANDIN_LITELLM_BASE_URL`, `STANDIN_LITELLM_KEY`,
   `STANDIN_MODEL` into `$GITHUB_ENV`.
5. `standin/smoke.sh` — parts (a), (b), (c) above.
6. `standin/env-gate.sh --require standin`.
7. **P1.5 hook**: the step named `Provider integration tests (P1.5 hook)` currently
   echoes the exported variables. When `crates/providers` lands, replace the echo
   with the real command (suggested: a `standin-integration` cargo feature or a test
   binary that reads the four `STANDIN_*` variables and fails, not skips, when they
   are unset). P3.4's waker test runs in the same job against the `slurm` service.
8. Always: compose logs on failure, `standin/up.sh --down`.

To run the same integration tests locally: `standin/up.sh`, `eval "$(standin/up.sh
--print-env)"`, then your `cargo test …` invocation.

## Environment-gated tier: real vLLM, real LiteLLM upstreams, real Slurm

The stand-in is what CI runs. Tests against real backends are ordinary tests that
**skip themselves when their gate variables are unset**; CI never sets them, humans
(or a self-hosted runner on the login node) do.

| Tier | Gate variables | Notes |
|---|---|---|
| vLLM | `GRIST_VLLM_BASE_URL` (`…/v1`), optional `GRIST_VLLM_API_KEY`, optional `GRIST_VLLM_MODEL` | served model name is whatever vLLM was started with |
| LiteLLM (real upstreams) | `GRIST_LITELLM_BASE_URL`, `GRIST_LITELLM_API_KEY`, optional `GRIST_LITELLM_MODEL` (`provider/model`) | spends real money; keep the model cheap |
| Slurm | `GRIST_REAL_SLURM=1` and a real `sbatch` on `PATH` | run on the login node; `env-gate.sh` refuses the fake one |
| Stand-in | `STANDIN_OPENAI_BASE_URL`, `STANDIN_LITELLM_BASE_URL`, `STANDIN_LITELLM_KEY`, `STANDIN_MODEL` | set by CI and `up.sh --print-env`; stand-in tests **fail** (not skip) when these are unset in CI |

Convention for the Rust side (cargo has no dynamic skip, so return early and say so):

```rust
/// Returns the gate value or prints a skip notice and returns None.
fn gate(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => { eprintln!("SKIPPED: {var} unset (environment-gated tier)"); None }
    }
}

#[tokio::test]
async fn vllm_tool_calls_roundtrip() {
    let Some(base) = gate("GRIST_VLLM_BASE_URL") else { return };
    // ...
}
```

Name gated tests with the tier as a prefix (`vllm_…`, `litellm_real_…`, `slurm_real_…`)
so `cargo test vllm_` runs one tier. `standin/env-gate.sh` prints which tiers the
current shell can reach and `--require` turns that into an exit code for scripts.

## Troubleshooting

- **llama never becomes healthy**: first start downloads ~1.1 GB from
  huggingface.co; `docker compose -f standin/compose.yaml logs llama`. Behind a proxy
  set `HTTPS_PROXY` for the container via `.env`. A partial download is resumed by
  llama.cpp; delete `standin/.cache/models/*` to start over.
- **`set LITELLM_MASTER_KEY`** error from compose: run `standin/up.sh` (creates `.env`)
  or copy `standin/.env.example`.
- **Ports in use**: set `STANDIN_LLAMA_PORT` / `STANDIN_LITELLM_PORT` in `.env` and
  re-run `standin/up.sh --print-env`.
- **No tool call in smoke (a)**: check the request has `tools` and llama runs with
  `--jinja` (compose does); the 1.5B model occasionally answers in prose at higher
  temperatures, hence the three attempts (first at temperature 0).
- **Cache permissions in CI**: the llama container writes as root; the workflow
  `chown`s `standin/.cache/models` before the cache post-step.
- **macOS / arm64**: both images are multi-arch; expect slower generation. Windows:
  WSL2 (D14).

## Status and open items

- Fake Slurm and mock solver: implemented and covered by tests that run on any
  Linux host (and in CI before the stack starts).
- Compose stack, LiteLLM routing, tool calling, SSE: authored and lint-checked;
  proven by the `standin` CI job (the authoring environment had no container
  runtime and no Hugging Face access).
- The P1.5 integration tests and the P3.4 waker test plug into the marked CI step.
- Model choice: Qwen2.5-1.5B is the tool-calling pin; a reasoning-capable swap
  (Qwen3) is documented above but not exercised in CI.
