#!/usr/bin/env bash
# STEP 15c FOLLOW-UP — regression test for the real bug found live on the
# VPS: benchmark-token.sh's `check` mode crashed on cast's default text
# output, which annotates large integers with a human-readable bracket
# (e.g. a real captured line was `1787557476 [1.787e9]`, not the bare
# `1787557476` the script originally assumed). Confirmed the underlying
# RPC call and values were correct both times — this was purely a
# parsing bug, and this test exists so it can't silently regress.
#
# No shell-script test harness existed anywhere in this repo before this
# file — confirmed by reading .github/workflows/ci.yml directly, not
# assumed: three jobs (Rust, UI, Secret scan), none touching deploy/*.sh
# at all. This is a minimal one, not a framework — plain bash, no bats
# or other new CI dependency, matching this project's own "don't add
# infrastructure the workload doesn't need" convention (see CLAUDE.md's
# CMS section for the same reasoning applied elsewhere).
#
# Runs entirely offline: `cast` is stubbed with a fake script that
# reproduces the exact output shape (including the bracket annotation)
# tonight's live crash captured — never calls a real RPC, never needs
# Foundry installed. Run manually with: ./deploy/tests/test-benchmark-token.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET="$SCRIPT_DIR/../benchmark-token.sh"
FAKE_BIN_DIR=$(mktemp -d)
trap 'rm -rf "$FAKE_BIN_DIR"' EXIT

FAILURES=0

run_check() {
  local addr="$1"
  PATH="$FAKE_BIN_DIR:$PATH" RPC_URL="http://fake-rpc.invalid" \
    bash "$TARGET" check "$addr"
}

assert_contains() {
  local haystack="$1" needle="$2" label="$3"
  if [[ "$haystack" != *"$needle"* ]]; then
    echo "FAIL: $label — expected output to contain '$needle'" >&2
    echo "--- actual output ---" >&2
    echo "$haystack" >&2
    FAILURES=$((FAILURES + 1))
    return 1
  fi
  echo "PASS: $label"
  return 0
}

echo "=== test 1: still-live drop, cast output with bracket annotations ==="
echo "    (this is the exact bug found live — a real captured line was"
echo "     '1787557476 [1.787e9]', not the bare integer the script"
echo "     originally assumed)"
cat > "$FAKE_BIN_DIR/cast" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == "call" ]]; then
  cat <<'OUT'
0
1787471076 [1.787e9]
1787557476 [1.787e9]
65535
0
false
OUT
fi
EOF
chmod +x "$FAKE_BIN_DIR/cast"
OUT=$(run_check "0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9") || {
  echo "FAIL: test 1 — script exited non-zero (this is the exact crash mode found live)" >&2
  FAILURES=$((FAILURES + 1))
}
assert_contains "$OUT" "STILL LIVE" "test 1: reports still-live without crashing on the bracket annotation"
assert_contains "$OUT" "endTime=1787557476" "test 1: extracts the bare integer, not the bracket-annotated string"

echo
echo "=== test 2: expired drop, same bracket-annotated format ==="
cat > "$FAKE_BIN_DIR/cast" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == "call" ]]; then
  cat <<'OUT'
0
1000000 [1e6]
1000100 [1.0001e6]
65535
0
false
OUT
fi
EOF
chmod +x "$FAKE_BIN_DIR/cast"
set +e
OUT=$(run_check "0xexpired")
CODE=$?
set -e
if [[ "$CODE" -eq 0 ]]; then
  echo "FAIL: test 2 — expected non-zero exit for an expired drop" >&2
  FAILURES=$((FAILURES + 1))
else
  echo "PASS: test 2: non-zero exit on an expired drop"
fi
assert_contains "$OUT" "EXPIRED" "test 2: reports EXPIRED without crashing on the bracket annotation"

echo
echo "=== test 3: no drop configured (all zeros, no bracket needed at this magnitude) ==="
cat > "$FAKE_BIN_DIR/cast" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == "call" ]]; then
  cat <<'OUT'
0
0
0
0
0
false
OUT
fi
EOF
chmod +x "$FAKE_BIN_DIR/cast"
set +e
OUT=$(run_check "0xnodrop")
CODE=$?
set -e
if [[ "$CODE" -eq 0 ]]; then
  echo "FAIL: test 3 — expected non-zero exit when no drop is configured" >&2
  FAILURES=$((FAILURES + 1))
else
  echo "PASS: test 3: non-zero exit when endTime is 0"
fi
assert_contains "$OUT" "unreadable" "test 3: reports the no-drop case clearly"

echo
if [[ "$FAILURES" -eq 0 ]]; then
  echo "ALL TESTS PASSED"
  exit 0
else
  echo "$FAILURES TEST(S) FAILED"
  exit 1
fi
