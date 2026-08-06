#!/bin/bash
# v7.39 (round 783) — the seven-gate runner, in the repo.
#
# It lived only in the testbed's /tmp for its whole life, so a testbed
# rebuild would silently drop gates (the round-751 uniq-prune step in
# particular). Keep it here; copy to the testbed and run from there:
#   scp scripts/gates.sh mini.local:/tmp/gates_remote.sh
#   ssh mini.local 'nohup zsh /tmp/gates_remote.sh > /tmp/gates.log 2>&1 &'
export PATH=/Applications/OrbStack.app/Contents/MacOS/xbin:$PATH
cd ~/spg || exit 1
echo "=== TESTS ==="
cargo test --release --workspace --no-fail-fast > /tmp/gt.log 2>&1
grep -E "^test .* FAILED" /tmp/gt.log | head -20
# v7.39 (round 783) — keep a failing run's full output and show the
# first panic. A flake whose diagnosis needs the panic text used to be
# undiagnosable: the next gate run overwrote /tmp/gt.log.
if grep -qE "^test .* FAILED" /tmp/gt.log; then
  keep=/tmp/gt-fail-$(date +%Y%m%d-%H%M%S).log
  cp /tmp/gt.log "$keep"
  echo "--- failing run preserved: $keep"
  grep -A12 "panicked at" /tmp/gt.log | head -24
fi
grep "test result" /tmp/gt.log | awk '{p+=$4; f+=$6} END {print "passed="p" failed="f}'
echo "=== CLIPPY (cold full tree) ==="
touch crates/*/src/lib.rs
cargo clippy --release --workspace --all-targets 2>&1 | grep -c "^warning:\|^error"
echo "=== CONFORMANCE ==="
# No pipe: a pipe hands the caller tail's status, not the runner's.
(cd xtests/sqllogictest && cargo run --release -q > /tmp/slt.log 2>&1)
slt_rc=$?
tail -12 /tmp/slt.log
[ $slt_rc -eq 0 ] || echo 'SLT GATE RED'
echo "=== DUMP-COMPAT ==="
./xtests/dump_compat/run.sh local-build > /tmp/gd.log 2>&1
grep -c PASS /tmp/gd.log; grep -c FAIL /tmp/gd.log
echo "=== DATA-COMPAT ==="
./xtests/data_compat/run.sh local-build 2>&1 | grep "|"
echo "=== CORPUS ==="
xtests/diffcorpus/run.sh; corpus_rc=$?
[ $corpus_rc -eq 0 ] || echo "CORPUS GATE RED"
echo "=== UNIQ-PRUNE (perf-counters, own process) ==="
cargo test --release -p spg-engine --test uniq_prune_counters --features perf-counters > /tmp/gu.log 2>&1
uniq_rc=$?
grep "test result" /tmp/gu.log
[ $uniq_rc -eq 0 ] || echo "UNIQ-PRUNE GATE RED"
echo GATES_DONE
