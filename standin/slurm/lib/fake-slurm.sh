# shellcheck shell=bash
# Shared library for the fake Slurm stand-in (P0.5, D18).
#
# Sourced by sbatch / squeue / scancel / sacct and the job runner. Everything
# lives in a state directory; one job = one directory of small text files:
#
#   $FAKE_SLURM_STATE_DIR/
#     counter                 next job id (flock-protected)
#     jobs/<id>/meta          key=value: name, user, partition, workdir, stdout, ...
#     jobs/<id>/args          one script argument per line
#     jobs/<id>/script        the batch script (or a generated one for --wrap)
#     jobs/<id>/state         PENDING | RUNNING | COMPLETED | FAILED | CANCELLED | TIMEOUT
#     jobs/<id>/runner.pid    pid of the detached runner
#     jobs/<id>/child.pid     pid of the batch script process (while RUNNING)
#     jobs/<id>/child.pgid    its process group (scancel kills the whole group)
#     jobs/<id>/exit_code     numeric exit status once finished
#     jobs/<id>/start_time / end_time   epoch seconds
#     jobs/<id>/cancel_requested        present once scancel was called
#     jobs/<id>/epilog.log / epilog.rc  output and exit status of the epilog hook
#
# Every write is a temp-file + mv so readers never see a torn file.

FAKE_SLURM_VERSION="23.11.4-fake"
FAKE_SLURM_STATE_DIR="${FAKE_SLURM_STATE_DIR:-${TMPDIR:-/tmp}/fake-slurm-$(id -u)}"
FAKE_SLURM_NODE="${FAKE_SLURM_NODE:-fakenode01}"
FAKE_SLURM_DEFAULT_PARTITION="${FAKE_SLURM_DEFAULT_PARTITION:-standin}"
# Finished jobs stay visible to squeue for this long (Slurm's MinJobAge).
FAKE_SLURM_MIN_JOB_AGE="${FAKE_SLURM_MIN_JOB_AGE:-300}"

fs_init() { mkdir -p "$FAKE_SLURM_STATE_DIR/jobs"; }
fs_jobdir() { printf '%s/jobs/%s' "$FAKE_SLURM_STATE_DIR" "$1"; }
fs_now() { date +%s; }
fs_user() { id -un 2>/dev/null || printf 'user%s' "$(id -u)"; }

# fs_write FILE CONTENT  -- atomic replace
fs_write() {
  local f="$1" tmp
  tmp="$1.tmp.$$"
  printf '%s\n' "$2" >"$tmp" && mv -f "$tmp" "$f"
}

# fs_read FILE [DEFAULT]
fs_read() {
  if [[ -f "$1" ]]; then
    # strip exactly one trailing newline, keep everything else
    local v
    v=$(<"$1")
    printf '%s' "$v"
  else
    printf '%s' "${2-}"
  fi
}

# fs_meta_get JOBDIR KEY [DEFAULT]
fs_meta_get() {
  local line
  if [[ -f "$1/meta" ]]; then
    line=$(grep -m1 "^$2=" "$1/meta" || true)
    if [[ -n "$line" ]]; then
      printf '%s' "${line#"$2="}"
      return 0
    fi
  fi
  printf '%s' "${3-}"
}

fs_next_id() {
  fs_init
  local lock="$FAKE_SLURM_STATE_DIR/counter.lock"
  (
    flock -w 10 9 || { echo "sbatch: error: could not lock state dir" >&2; exit 1; }
    local cur
    cur=$(fs_read "$FAKE_SLURM_STATE_DIR/counter" 1000)
    cur=$((cur + 1))
    fs_write "$FAKE_SLURM_STATE_DIR/counter" "$cur"
    printf '%s\n' "$cur"
  ) 9>"$lock"
}

fs_job_exists() { [[ -d "$(fs_jobdir "$1")" && -f "$(fs_jobdir "$1")/meta" ]]; }

# A RUNNING job whose runner vanished (kill -9, machine reboot) would otherwise
# stay RUNNING forever. Slurm reports such jobs as NODE_FAIL; we use FAILED.
fs_refresh_job() {
  local d="$1" st pid
  st=$(fs_read "$d/state")
  case "$st" in
    PENDING|RUNNING)
      pid=$(fs_read "$d/runner.pid")
      if [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null; then
        fs_write "$d/exit_code" 1
        fs_write "$d/end_time" "$(fs_now)"
        fs_write "$d/state" FAILED
      fi
      ;;
  esac
}

fs_state_code() {
  case "$1" in
    PENDING) printf 'PD' ;;
    RUNNING) printf 'R' ;;
    COMPLETED) printf 'CD' ;;
    FAILED) printf 'F' ;;
    CANCELLED) printf 'CA' ;;
    TIMEOUT) printf 'TO' ;;
    *) printf '??' ;;
  esac
}

# fs_fmt_elapsed SECONDS -> squeue-style  M:SS | H:MM:SS | D-HH:MM:SS
fs_fmt_elapsed() {
  local s=$1 d h m
  d=$((s / 86400)); s=$((s % 86400))
  h=$((s / 3600)); s=$((s % 3600))
  m=$((s / 60)); s=$((s % 60))
  if ((d > 0)); then printf '%d-%02d:%02d:%02d' "$d" "$h" "$m" "$s"
  elif ((h > 0)); then printf '%d:%02d:%02d' "$h" "$m" "$s"
  else printf '%d:%02d' "$m" "$s"; fi
}

# fs_fmt_timestamp EPOCH -> 2026-09-06T12:34:56  (sacct style), or "Unknown"
fs_fmt_timestamp() {
  if [[ -n "$1" ]]; then date -d "@$1" +%Y-%m-%dT%H:%M:%S; else printf 'Unknown'; fi
}

# fs_job_elapsed JOBDIR -> seconds spent RUNNING so far (0 if pending)
fs_job_elapsed() {
  local d="$1" st start end
  st=$(fs_read "$d/state")
  start=$(fs_read "$d/start_time")
  [[ -z "$start" ]] && { printf 0; return; }
  case "$st" in
    RUNNING) end=$(fs_now) ;;
    *) end=$(fs_read "$d/end_time" "$(fs_now)") ;;
  esac
  printf '%s' $((end - start))
}

# fs_parse_time SPEC -> seconds.  Slurm forms: M, M:S, H:M:S, D-H, D-H:M, D-H:M:S
fs_parse_time() {
  local spec="$1" days=0 rest h=0 m=0 s=0
  [[ -z "$spec" || "$spec" == "0" || "$spec" == "UNLIMITED" ]] && { printf 0; return; }
  if [[ "$spec" == *-* ]]; then
    days=${spec%%-*}; rest=${spec#*-}
    IFS=: read -r h m s <<<"$rest"
  else
    local n
    n=$(awk -F: '{print NF}' <<<"$spec")
    case "$n" in
      1) m=$spec ;;
      2) IFS=: read -r m s <<<"$spec" ;;
      *) IFS=: read -r h m s <<<"$spec" ;;
    esac
  fi
  printf '%s' $((days * 86400 + ${h:-0} * 3600 + ${m:-0} * 60 + ${s:-0}))
}

# fs_expand_pattern PATTERN JOBID NAME USER -> expands Slurm filename patterns
fs_expand_pattern() {
  local p="$1" id="$2" name="$3" user="$4"
  p=${p//%%/$'\x01'}
  p=${p//%j/$id}
  p=${p//%J/$id}
  p=${p//%A/$id}
  p=${p//%a/0}
  p=${p//%x/$name}
  p=${p//%u/$user}
  p=${p//%N/$FAKE_SLURM_NODE}
  p=${p//%n/0}
  p=${p//%t/0}
  p=${p//$'\x01'/%}
  printf '%s' "$p"
}

# fs_all_job_ids -> numerically sorted ids
fs_all_job_ids() {
  fs_init
  local d
  for d in "$FAKE_SLURM_STATE_DIR"/jobs/*/; do
    [[ -d "$d" ]] || continue
    d=${d%/}
    printf '%s\n' "${d##*/}"
  done | sort -n
}

# fs_in_list NEEDLE COMMA_LIST
fs_in_list() {
  local IFS=,
  local x
  for x in $2; do [[ "$x" == "$1" ]] && return 0; done
  return 1
}

fs_die() { printf '%s\n' "$*" >&2; exit 1; }
