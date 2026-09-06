#!/usr/bin/env bash
# shellcheck disable=SC2034  # variables are read inside eval'd check() expressions
# Self-test for the fake Slurm stand-in. Uses a throwaway state dir and epilog.
# Proves: submit -> squeue RUNNING -> COMPLETED with epilog (job id + exit code),
# failure exit code propagation, scancel, #SBATCH directives + %x/%j patterns,
# --wrap/--parsable/--wait, SLURM_* env inside the job, time-limit -> TIMEOUT,
# sacct accounting, and that a cancelled job's process tree is gone.
set -euo pipefail

here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
export PATH="$here/bin:$PATH"
work=$(mktemp -d "${TMPDIR:-/tmp}/fake-slurm-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
cd "$work"
export FAKE_SLURM_STATE_DIR="$work/state"
export FAKE_SLURM_EPILOG="$work/epilog.sh"
unset FAKE_SLURM_PENDING_SECONDS
export FAKE_SLURM_KILL_WAIT=2

cat >"$FAKE_SLURM_EPILOG" <<'EPI'
#!/bin/sh
# Epilog hook: record what the waker (P3.4) will see.
printf 'id=%s rc=%s ec2=%s state=%s name=%s stdout=%s\n' \
  "$SLURM_JOB_ID" "$SLURM_JOB_EXIT_CODE" "$SLURM_JOB_EXIT_CODE2" "$FAKE_SLURM_JOB_STATE" \
  "$SLURM_JOB_NAME" "$SLURM_JOB_STDOUT" >>"$FAKE_SLURM_STATE_DIR/../epilog.log"
EPI
chmod +x "$FAKE_SLURM_EPILOG"

pass=0; fail=0
ok()   { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf '  FAIL %s\n' "$1"; }
check() { if eval "$2"; then ok "$1"; else bad "$1 -- [$2]"; fi; }
wait_state() {  # wait_state ID STATE [TIMEOUT_S]
  local id="$1" want="$2" t="${3:-20}" st="" i
  for ((i = 0; i < t * 10; i++)); do
    st=$(squeue -h -t all -j "$id" -o %T)
    [[ "$st" == "$want" ]] && return 0
    sleep 0.1
  done
  echo "    (last state: $st)"; return 1
}
live_in_group() {  # true if any non-zombie process is in process group $1
  local p
  for p in $(pgrep -g "$1" 2>/dev/null); do
    [[ "$(ps -o stat= -p "$p" 2>/dev/null)" == Z* ]] || return 0
  done
  return 1
}
epilog_line() { grep -m1 "^id=$1 " "$work/epilog.log" 2>/dev/null || true; }

echo "== 1. submit, squeue RUNNING, completion, epilog"
cat >job.sh <<'JOB'
#!/bin/bash
#SBATCH --job-name=hello
#SBATCH -o %x-%j.out
#SBATCH --ntasks=2 --cpus-per-task=3
echo "id=$SLURM_JOB_ID name=$SLURM_JOB_NAME node=$SLURM_JOB_NODELIST ntasks=$SLURM_NTASKS cpus=$SLURM_CPUS_PER_TASK submit=$SLURM_SUBMIT_DIR args=$*"
sleep 1.5
echo done
JOB
out=$(sbatch job.sh alpha "beta gamma")
check "sbatch prints 'Submitted batch job <id>'" '[[ "$out" =~ ^Submitted\ batch\ job\ [0-9]+$ ]]'
id=${out##* }
check "job reaches RUNNING" 'wait_state "$id" RUNNING 5'
check "squeue default view lists it with ST=R" 'squeue -h | grep -qE "^ *$id +standin +hello +[^ ]+ +R "'
check "job reaches COMPLETED" 'wait_state "$id" COMPLETED 10'
check "stdout follows the %x-%j pattern" '[[ -f "hello-$id.out" ]]'
check "SLURM_* env visible inside the job" 'grep -q "id=$id name=hello node=fakenode01 ntasks=2 cpus=3 submit=$work args=alpha beta gamma" "hello-$id.out"'
check "job output complete" 'grep -q "^done$" "hello-$id.out"'
check "epilog fired with job id and exit code 0" '[[ "$(epilog_line "$id")" == "id=$id rc=0 ec2=0:0 state=COMPLETED name=hello stdout=$work/hello-$id.out" ]]'
check "epilog exit status recorded" '[[ "$(cat "$FAKE_SLURM_STATE_DIR/jobs/$id/epilog.rc")" == 0 ]]'
check "sacct shows COMPLETED 0:0" '[[ "$(sacct -n -P -j "$id" -o State,ExitCode)" == "COMPLETED|0:0" ]]'
check "sacct AllocCPUS = ntasks*cpus" '[[ "$(sacct -n -P -j "$id" -o AllocCPUS)" == 6 ]]'

echo "== 2. failure path: exit code propagates to state, epilog, sacct, --wait"
set +e
out=$(sbatch --wait --parsable --job-name=boom --wrap 'echo failing; exit 7' 2>err.txt); rc=$?
set -e
id=$out
check "--parsable prints only the id" '[[ "$id" =~ ^[0-9]+$ ]]'
check "--wait returns the job exit code (7)" '[[ $rc == 7 ]]'
check "state is FAILED" '[[ "$(squeue -h -t all -j "$id" -o %T)" == FAILED ]]'
check "default stdout slurm-<id>.out written" 'grep -q "^failing$" "slurm-$id.out"'
check "epilog got exit code 7 and FAILED" '[[ "$(epilog_line "$id")" == "id=$id rc=7 ec2=7:0 state=FAILED name=boom stdout=$work/slurm-$id.out" ]]'
check "sacct ExitCode 7:0" '[[ "$(sacct -n -P -j "$id" -o State,ExitCode)" == "FAILED|7:0" ]]'
check "sbatch --wait reported the failure on stderr" 'grep -q "job $id FAILED" err.txt'

echo "== 3. scancel a running job (TERM honoured)"
id=$(sbatch --parsable --job-name=victim --wrap 'exec sleep 60')
check "job running" 'wait_state "$id" RUNNING 5'
child=$(cat "$FAKE_SLURM_STATE_DIR/jobs/$id/child.pid")
pgid=$(cat "$FAKE_SLURM_STATE_DIR/jobs/$id/child.pgid")
check "batch script runs in its own process group" '[[ "$pgid" == "$child" ]]'
t_cancel=$(date +%s%N)
scancel "$id"
check "state becomes CANCELLED" 'wait_state "$id" CANCELLED 5'
check "cancellation was prompt (SIGTERM, not the KILL escalation)" '(( $(date +%s%N) - t_cancel < 1500000000 ))'
check "Slurm-style cancel trailer appended to stdout" 'grep -q "\*\*\* JOB $id ON fakenode01 CANCELLED AT" "slurm-$id.out"'
check "epilog fired with state CANCELLED and signal 15" '[[ "$(epilog_line "$id")" == "id=$id rc=143 ec2=0:15 state=CANCELLED name=victim stdout=$work/slurm-$id.out" ]]'
check "no live process from the job's group survives" '! live_in_group "$pgid"'
check "sacct State 'CANCELLED by <uid>' and ExitCode 0:15" '[[ "$(sacct -n -P -j "$id" -o State,ExitCode)" == "CANCELLED by $(id -u)|0:15" ]]'
check "scancel of a finished job errors like Slurm" '! scancel "$id" 2>/dev/null'
check "scancel of an unknown job errors" '! scancel 999999 2>/dev/null'

echo "== 3b. scancel a job that ignores SIGTERM (KILL escalation after FAKE_SLURM_KILL_WAIT)"
id=$(sbatch --parsable --wrap 'trap "" TERM; sleep 60 & wait')
check "job running" 'wait_state "$id" RUNNING 5'
pgid=$(cat "$FAKE_SLURM_STATE_DIR/jobs/$id/child.pgid")
scancel "$id"
check "state becomes CANCELLED after the KILL escalation" 'wait_state "$id" CANCELLED 10'
check "epilog reports signal 9" '[[ "$(epilog_line "$id")" == *"rc=137 ec2=0:9 state=CANCELLED"* ]]'
sleep 0.3
check "no live process from the job's group survives" '! live_in_group "$pgid"'
check "sacct ExitCode 0:9" '[[ "$(sacct -n -P -j "$id" -o ExitCode)" == "0:9" ]]'

echo "== 4. cancel while PENDING (no epilog, like Slurm)"
id=$(FAKE_SLURM_PENDING_SECONDS=30 sbatch --parsable --wrap 'echo never')
check "job is PENDING" 'wait_state "$id" PENDING 5'
check "squeue shows PD with (Priority) reason" 'squeue -h -j "$id" -o "%t %R" | grep -q "^PD (Priority)$"'
scancel "$id"
check "state becomes CANCELLED" 'wait_state "$id" CANCELLED 5'
check "script never ran" '[[ ! -f "slurm-$id.out" ]]'
check "no epilog for a job that never started" '[[ -z "$(epilog_line "$id")" ]]'

echo "== 5. time limit -> TIMEOUT"
id=$(sbatch --parsable --time=00:01 --wrap 'sleep 30')
check "state becomes TIMEOUT" 'wait_state "$id" TIMEOUT 15'
check "timeout trailer in stdout" 'grep -q "DUE TO TIME LIMIT" "slurm-$id.out"'
check "epilog fired with TIMEOUT" '[[ "$(epilog_line "$id")" == *"state=TIMEOUT"* ]]'

echo "== 6. options: --chdir, --output absolute, --error separate, --export=NONE, stdin script"
mkdir -p sub
id=$(sbatch --parsable --chdir sub -o "$work/abs-%j.log" -e "$work/abs-%j.err" --export=NONE --wrap 'pwd; echo "home=$HOME shell=$SHELL x=${MY_SECRET-unset}"; echo oops >&2')
MY_SECRET=leak sbatch --parsable --wrap 'true' >/dev/null
check "job completes" 'wait_state "$id" COMPLETED 10'
check "--chdir sets the working directory" 'grep -qx "$work/sub" "abs-$id.log"'
check "--export=NONE scrubs the caller environment" 'grep -q "x=unset" "abs-$id.log"'
check "stderr separated by --error" 'grep -qx oops "abs-$id.err" && ! grep -q oops "abs-$id.log"'
id=$(printf '#!/bin/sh\n#SBATCH -J fromstdin\necho stdin-ok\n' | sbatch --parsable)
check "script on stdin accepted" 'wait_state "$id" COMPLETED 10 && grep -q stdin-ok "slurm-$id.out"'
check "#SBATCH -J from a stdin script honoured" '[[ "$(squeue -h -t all -j "$id" -o %j)" == fromstdin ]]'

echo "== 7. error handling"
check "unknown option is rejected" '! sbatch --bogus job.sh 2>/dev/null'
check "missing script is rejected" '! sbatch /no/such/script.sh 2>/dev/null'
check "--array is rejected (unsupported)" '! sbatch --array=1-3 job.sh 2>/dev/null'
check "versions" '[[ "$(sbatch --version)" == "slurm 23.11.4-fake" && "$(squeue --version)" == "slurm 23.11.4-fake" ]]'

echo "== 8. squeue filtering and format widths"
n_all=$(squeue -h -t all | wc -l)
check "squeue -t all lists every job" '(( n_all >= 8 ))'
check "squeue -t COMPLETED filters by state" '[[ "$(squeue -h -t COMPLETED -o %T | sort -u)" == COMPLETED ]]'
check "custom format with width" '[[ "$(squeue -h -t all -j 1001 -o "%.6i|%8j|%t")" == "  1001|hello   |CD" ]]'
check "unknown option rejected" '! squeue --nonsense 2>/dev/null'

echo
echo "fake-slurm self-test: $pass passed, $fail failed"
(( fail == 0 ))
