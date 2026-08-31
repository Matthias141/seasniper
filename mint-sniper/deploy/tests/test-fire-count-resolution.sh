#!/usr/bin/env bash
# STEP 32a/32d — regression test for run-benchmark.sh's two small bash
# idioms this step added: (1) N's precedence (positional arg wins, else
# FIRE_COUNT env var, else 15 — the existing default, unchanged for
# anyone who sets neither), and (2) the sizing-warning threshold +
# rough-total-ETH arithmetic (STEP 32d). run-benchmark.sh itself needs
# root/systemd/a real bot and can't be invoked directly in CI (same
# reason test-config-backup-restore.sh replicates its target idioms
# against scratch data rather than running the real script) — this
# tests the EXACT expressions run-benchmark.sh uses, copied verbatim,
# not a paraphrase of them. Run manually with:
#   ./deploy/tests/test-fire-count-resolution.sh

set -euo pipefail

FAILURES=0

check() {
  local condition="$1" label="$2"
  if eval "$condition"; then
    echo "PASS: $label"
  else
    echo "FAIL: $label" >&2
    FAILURES=$((FAILURES + 1))
  fi
}

echo "=== N precedence: positional arg > FIRE_COUNT env var > default 15 ==="
# The exact expression run-benchmark.sh uses: N="${1:-${FIRE_COUNT:-15}}"

resolve_n() {
  local pos="${1:-}"
  local N
  N="${pos:-${FIRE_COUNT:-15}}"
  echo "$N"
}

unset FIRE_COUNT || true
check '[[ "$(resolve_n)" == "15" ]]' "neither positional arg nor FIRE_COUNT set -> defaults to 15 (unchanged prior behavior)"

FIRE_COUNT=100
check '[[ "$(resolve_n)" == "100" ]]' "FIRE_COUNT=100, no positional arg -> 100"

FIRE_COUNT=100
check '[[ "$(resolve_n 25)" == "25" ]]' "FIRE_COUNT=100 AND a positional arg of 25 -> positional arg wins (25)"

unset FIRE_COUNT || true
check '[[ "$(resolve_n 42)" == "42" ]]' "positional arg only (no FIRE_COUNT) -> 42"

echo
echo "=== sizing-warning threshold: exact condition and rough-total math ==="
# The exact condition run-benchmark.sh uses: (( N > FIRE_COUNT_WARN_THRESHOLD ))
FIRE_COUNT_WARN_THRESHOLD=20

n_triggers_warning() {
  local N="$1"
  (( N > FIRE_COUNT_WARN_THRESHOLD ))
}

if n_triggers_warning 15; then check 'false' "n=15 (the default) does NOT trigger the sizing warning"; else check 'true' "n=15 (the default) does NOT trigger the sizing warning"; fi
if n_triggers_warning 20; then check 'false' "n=20 (exactly at the threshold) does NOT trigger the warning (strict greater-than)"; else check 'true' "n=20 (exactly at the threshold) does NOT trigger the warning (strict greater-than)"; fi
if n_triggers_warning 21; then check 'true' "n=21 (just above the threshold) DOES trigger the warning"; else check 'false' "n=21 (just above the threshold) DOES trigger the warning"; fi
if n_triggers_warning 100; then check 'true' "n=100 (a real benchmark run) DOES trigger the warning"; else check 'false' "n=100 (a real benchmark run) DOES trigger the warning"; fi

echo
echo "=== rough-total-ETH arithmetic (the exact python3 -c call run-benchmark.sh uses) ==="
ROUGH_GAS_COST_PER_FIRE_ETH="0.00002"

rough_total() {
  local N="$1"
  python3 -c "print(f'{${N} * ${ROUGH_GAS_COST_PER_FIRE_ETH}:.5f}')"
}

check '[[ "$(rough_total 100)" == "0.00200" ]]' "n=100 -> 0.00200 ETH rough total"
check '[[ "$(rough_total 25)" == "0.00050" ]]' "n=25 -> 0.00050 ETH rough total"
check '[[ "$(rough_total 1)" == "0.00002" ]]' "n=1 -> 0.00002 ETH (the per-fire estimate itself)"

echo
if [[ "$FAILURES" -eq 0 ]]; then
  echo "ALL TESTS PASSED"
  exit 0
else
  echo "$FAILURES TEST(S) FAILED"
  exit 1
fi
