#!/usr/bin/env bash
# STEP 27 — regression test for deploy/lib/resolve_benchmark_address.sh
# against the real bug: run-benchmark.sh's BENCHMARK_CHECK_ADDR used to
# have a single hardcoded default (step 14b's original benchmark token)
# with no mechanism to pick up a later `benchmark-token.sh redeploy`'s
# fresh address -- an operator following run-benchmark.sh's own "Usage:"
# example literally would silently re-check a STALE address every run.
# This tests the precedence the fix introduces: env override > the state
# file `benchmark-token.sh redeploy` now writes on success > the original
# hardcoded fallback. Run manually with:
#   ./deploy/tests/test-resolve-benchmark-address.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESOLVE_SCRIPT="$SCRIPT_DIR/../lib/resolve_benchmark_address.sh"
SCRATCH=$(mktemp -d)
trap 'rm -rf "$SCRATCH"' EXIT
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

HARDCODED_DEFAULT="0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9"
STEP26_ADDR="0x118fafd8511a04Df686e848425253c838B3a1a94"
STATE_FILE="$SCRATCH/.benchmark-token-address"

echo "=== no state file, no env var — falls back to the original hardcoded default ==="
RESULT=$("$RESOLVE_SCRIPT" "$STATE_FILE" "")
check '[[ "$RESULT" == "$HARDCODED_DEFAULT hardcoded-default" ]]' "resolves to the hardcoded default with the right source tag"

echo
echo "=== state file exists (benchmark-token.sh redeploy ran) — used automatically, no env var needed ==="
echo "$STEP26_ADDR" > "$STATE_FILE"
RESULT=$("$RESOLVE_SCRIPT" "$STATE_FILE" "")
check '[[ "$RESULT" == "$STEP26_ADDR state-file" ]]' "auto-discovers the redeployed address from the state file (the real step 27 fix)"

echo
echo "=== env var explicitly set — always wins, even with a state file present ==="
OVERRIDE_ADDR="0x000000000000000000000000000000deadbeef"
RESULT=$("$RESOLVE_SCRIPT" "$STATE_FILE" "$OVERRIDE_ADDR")
check '[[ "$RESULT" == "$OVERRIDE_ADDR env" ]]' "an explicit BENCHMARK_CHECK_ADDR override always wins"

echo
echo "=== state file exists but is empty/garbage — falls back to hardcoded default, not a crash ==="
echo "not an address" > "$STATE_FILE"
RESULT=$("$RESOLVE_SCRIPT" "$STATE_FILE" "")
check '[[ "$RESULT" == "$HARDCODED_DEFAULT hardcoded-default" ]]' "an unreadable state file degrades to the hardcoded default instead of erroring"

if [[ "$FAILURES" -eq 0 ]]; then
  echo
  echo "ALL TESTS PASSED"
  exit 0
else
  echo
  echo "$FAILURES TEST(S) FAILED"
  exit 1
fi
