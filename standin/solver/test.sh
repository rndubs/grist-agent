#!/usr/bin/env bash
# shellcheck disable=SC2034  # variables are read inside eval'd check() expressions
# Self-test for the ACME mock solver: runs the three example decks with a tiny
# per-step sleep and asserts exit codes, log markers, and the result file.
set -euo pipefail

here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
solve="${ACME_SOLVE:-$here/opt/acme-solver/bin/solve}"
decks="$(dirname "$solve")/../share/decks"
work=$(mktemp -d "${TMPDIR:-/tmp}/acme-solver-test.XXXXXX")
trap 'rm -rf "$work"' EXIT
cd "$work"

pass=0; fail=0
ok()   { pass=$((pass + 1)); printf '  ok   %s\n' "$1"; }
bad()  { fail=$((fail + 1)); printf '  FAIL %s\n' "$1"; }
check() { if eval "$2"; then ok "$1"; else bad "$1 -- [$2]"; fi; }
json_field() { python3 -c 'import json,sys; v=json.load(open(sys.argv[1]))[sys.argv[2]]; print("" if v is None else v)' "$1" "$2"; }

run_deck() {  # run_deck NAME -> sets rc, writes NAME.log and NAME/acme-result.json
  set +e
  "$solve" "$decks/$1.deck" --outdir "$1" --step-seconds 0.01 >"$1.log" 2>&1
  rc=$?
  set -e
}

echo "== converge.deck"
run_deck converge
check "exit code 0" '[[ $rc == 0 ]]'
check "header banner present" 'grep -q "ACME Solver 7.4.2" converge.log'
check "deck echoed" 'grep -q "name              = cantilever-static" converge.log'
check "10 step headers" '[[ $(grep -c "^--- step" converge.log) == 10 ]]'
check "per-iteration residual lines (>= 20)" '(( $(grep -c "resid = " converge.log) >= 20 ))'
check "every step reports convergence with a timing" '[[ $(grep -cE "converged in [0-9]+ iterations +\([0-9.]+ s\)" converge.log) == 10 ]]'
check "summary block with wall time" 'grep -q "^Summary$" converge.log && grep -qE "wall time +: [0-9.]+ s" converge.log'
check "final status line" 'grep -q "^Run status: CONVERGED$" converge.log'
check "no error codes emitted" '! grep -qE "ACME-E[0-9]+" converge.log'
check "result file written" '[[ -f converge/acme-result.json ]]'
check "result status CONVERGED, 10/10 steps, exit 0" '[[ "$(json_field converge/acme-result.json status)" == CONVERGED && "$(json_field converge/acme-result.json steps_completed)" == 10 && "$(json_field converge/acme-result.json exit_code)" == 0 ]]'
check "final residual below tolerance" 'python3 -c "import json,sys; sys.exit(0 if json.load(open(\"converge/acme-result.json\"))[\"final_residual\"] < 1e-6 else 1)"'

echo "== diverge.deck"
run_deck diverge
check "exit code 2" '[[ $rc == 2 ]]'
check "steps 1-2 converge before divergence starts" '[[ $(grep -c "converged in" diverge.log) == 2 ]]'
check "residuals grow monotonically in the diverging step" 'python3 - <<PY
import re
lines=open("diverge.log").read().split("--- step      3")[1].splitlines()
r=[float(m.group(1)) for l in lines if (m:=re.search(r"resid = ([0-9.e+-]+)",l))]
raise SystemExit(0 if len(r)>=3 and all(b>a for a,b in zip(r,r[1:])) else 1)
PY'
check "ACME-E201 divergence message with step and threshold" 'grep -qE "^ACME-E201 solution diverged at step 3 iter [0-9]+: residual [0-9.e+]+ exceeds 1.000e\+10" diverge.log'
check "final status DIVERGED" 'grep -q "^Run status: DIVERGED$" diverge.log'
check "result status DIVERGED with error, exit 2" '[[ "$(json_field diverge/acme-result.json status)" == DIVERGED && "$(json_field diverge/acme-result.json exit_code)" == 2 && "$(json_field diverge/acme-result.json error)" == "ACME-E201 diverged at step 3" ]]'
check "result records 2 completed steps" '[[ "$(json_field diverge/acme-result.json steps_completed)" == 2 ]]'

echo "== crash.deck"
run_deck crash
check "exit code 3" '[[ $rc == 3 ]]'
check "3 steps completed before the crash" '[[ $(grep -c "converged in" crash.log) == 3 ]]'
check "ACME-E303 fatal line names step 4" 'grep -qE "^ACME-E303 fatal: negative Jacobian in element [0-9]+ \(step 4\)$" crash.log'
check "abort marker and fake traceback" 'grep -q "^\*\*\* ABORT \*\*\*$" crash.log && grep -q "acme::fem::assemble_tangent" crash.log'
check "final status ABORTED" 'grep -q "^Run status: ABORTED$" crash.log'
check "result status ABORTED, exit 3, 3 steps" '[[ "$(json_field crash/acme-result.json status)" == ABORTED && "$(json_field crash/acme-result.json exit_code)" == 3 && "$(json_field crash/acme-result.json steps_completed)" == 3 ]]'

echo "== other modes"
printf 'steps = 3\nmax_iters = 3\nconvergence_rate = 0.9\n' >stall.deck
set +e; "$solve" stall.deck --outdir stall --step-seconds 0.01 >stall.log 2>&1; rc=$?; set -e
check "not-converged deck exits 4 with ACME-W410" '[[ $rc == 4 ]] && grep -q "ACME-W410 step 1 did not converge" stall.log && grep -q "^Run status: NOT_CONVERGED$" stall.log'
printf 'bogus_key = 1\n' >bad.deck
set +e; "$solve" bad.deck >bad.log 2>&1; rc=$?; set -e
check "unknown deck key exits 1 with ACME-E100" '[[ $rc == 1 ]] && grep -q "ACME-E100 deck error" bad.log'
set +e; "$solve" /no/such.deck >missing.log 2>&1; rc=$?; set -e
check "missing deck exits 1" '[[ $rc == 1 ]]'
check "--version" '[[ "$("$solve" --version)" == "ACME Solver 7.4.2 (standin)" ]]'
check "ACME_STEP_SECONDS env override honoured (run is fast)" 'start=$(date +%s%N); ACME_STEP_SECONDS=0.001 "$solve" "$decks/converge.deck" --outdir envfast >/dev/null; (( $(date +%s%N) - start < 3000000000 ))'
check "--log duplicates output to a file" '"$solve" "$decks/converge.deck" --outdir logdup --step-seconds 0.001 --log dup.log >/dev/null && grep -q "Run status: CONVERGED" dup.log'
check "deterministic log for a given deck (residuals identical across runs)" '"$solve" "$decks/converge.deck" --outdir d1 --step-seconds 0.001 | grep "resid =" >r1.txt; "$solve" "$decks/converge.deck" --outdir d2 --step-seconds 0.001 | grep "resid =" >r2.txt; cmp -s r1.txt r2.txt'
echo "== SIGTERM (what scancel delivers)"
"$solve" "$decks/converge.deck" --outdir term --step-seconds 2 >term.log 2>&1 &
pid=$!; sleep 0.5; kill -TERM $pid; set +e; wait $pid; rc=$?; set -e
check "SIGTERM -> ACME-W900 checkpoint line, INTERRUPTED, exit 143" '[[ $rc == 143 ]] && grep -q "ACME-W900 received signal 15" term.log && grep -q "^Run status: INTERRUPTED$" term.log && [[ "$(json_field term/acme-result.json status)" == INTERRUPTED ]]'

echo
echo "acme-solver self-test: $pass passed, $fail failed"
(( fail == 0 ))
