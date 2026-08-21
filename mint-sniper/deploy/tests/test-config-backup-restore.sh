#!/usr/bin/env bash
# STEP 15e FOLLOW-UP — regression test for the two safety-critical bash
# idioms run-benchmark.sh's cleanup trap and pre-flight guard depend on:
# (1) refusing to proceed when config.toml.backup already exists (the
# guard that prevents a second run from destroying a real backup left
# behind by a crashed prior run), and (2) byte-for-byte restore
# verification via `cmp -s` (the guard that must catch a failed/partial
# restore before the script is allowed to report success). Both tested
# against scratch files — no systemd, sudo, or real bot needed, since
# these two checks never touch either. Run manually with:
#   ./deploy/tests/test-config-backup-restore.sh

set -euo pipefail

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

echo "=== backup-exists guard: the exact condition run-benchmark.sh checks ==="
CONFIG="$SCRATCH/config.toml"
BACKUP="$SCRATCH/config.toml.backup"
echo "original content" > "$CONFIG"

check '[[ ! -f "$BACKUP" ]]' "no backup present yet — a fresh run would proceed"
touch "$BACKUP"
check '[[ -f "$BACKUP" ]]' "backup now present — a second run's guard would refuse to proceed (same [[ -f ]] check run-benchmark.sh uses before ever touching config.toml)"
rm -f "$BACKUP"

echo
echo "=== restore verification: cmp -s correctly detects a clean restore ==="
echo "original content" > "$CONFIG"
cp "$CONFIG" "$BACKUP"
echo "some testnet-swapped content" > "$CONFIG"   # simulates step 2's swap
cp "$BACKUP" "$CONFIG"                             # simulates step 5's restore
check 'cmp -s "$BACKUP" "$CONFIG"' "cmp -s reports a match after a clean restore — this is what lets the script report success"

echo
echo "=== restore verification: cmp -s correctly catches a FAILED/partial restore ==="
echo "original content" > "$CONFIG"
cp "$CONFIG" "$BACKUP"
echo "corrupted during a simulated partial write" > "$CONFIG"
check '! cmp -s "$BACKUP" "$CONFIG"' "cmp -s reports a MISMATCH when the restore didn't actually take — this is what makes the script fail loudly instead of reporting false success"

echo
if [[ "$FAILURES" -eq 0 ]]; then
  echo "ALL TESTS PASSED"
  exit 0
else
  echo "$FAILURES TEST(S) FAILED"
  exit 1
fi
