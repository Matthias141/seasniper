#!/usr/bin/env bash
# STEP 15e FOLLOW-UP — regression test for deploy/lib/find_mint_result.sh
# against a fixture reconstructed directly from audit.rs's own
# AuditRecord struct (#[serde(flatten)] on `detail`) and the
# MintResult match arm's json!({...}) — not a guess, and not a
# paraphrase of the live crash report either: the exact field shapes
# below are what that Rust code actually serializes. This is the real
# bug that crashed a live n=15 benchmark run on fire 1/15 with:
#   jq: error (at <stdin>:1): Cannot index string with string "success"
# while the underlying mint pipeline itself was confirmed correct
# (journalctl showed every wallet firing and confirming via the real WS
# PUSH path). Run manually with:
#   ./deploy/tests/test-find-mint-result.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIND_SCRIPT="$SCRIPT_DIR/../lib/find_mint_result.sh"
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

if ! command -v jq &>/dev/null; then
  echo "jq is required for this test and not on PATH" >&2
  exit 1
fi

echo "=== a real-shaped mint_result record (address/tx details redacted, everything else exact) ==="
AUDIT_LOG="$SCRATCH/audit.log"
# Note: no nested "detail" object at all — success/send_to_ack_ms/
# dispatch_to_inclusion_ms sit at the TOP level, flattened alongside a
# SEPARATE string-valued "detail" field ("confirmed") that shares its
# name with the outer Rust struct field but is NOT the same thing —
# exactly the collision that broke the original .detail.success code.
cat > "$AUDIT_LOG" <<'EOF'
{"ts":1755800000,"event":"arm","address":null}
{"ts":1755800001,"event":"mint_result","address":"0xREDACTED000000000000000000000000000000","success":true,"detail":"confirmed","trigger_to_dispatch_ms":4,"send_to_ack_ms":139,"dispatch_to_inclusion_ms":978,"prepare_age_ms":30}
EOF

RESULT=$("$FIND_SCRIPT" "$AUDIT_LOG" 1755800001)
check '[[ -n "$RESULT" ]]' "finds the matching mint_result record"
check '[[ "$(echo "$RESULT" | jq -r ".success")" == "true" ]]' "extracts success=true without crashing (the exact live failure)"
check '[[ "$(echo "$RESULT" | jq -r ".send_to_ack_ms")" == "139" ]]' "extracts the real send_to_ack_ms value"
check '[[ "$(echo "$RESULT" | jq -r ".dispatch_to_inclusion_ms")" == "978" ]]' "extracts the real dispatch_to_inclusion_ms value"
check '[[ "$(echo "$RESULT" | jq -r ".detail // \"absent\"")" == "absent" ]]' "the projected output does NOT carry the confusingly-named string 'detail' field forward"

echo
echo "=== a failed/reverted mint (success=false) ==="
cat > "$AUDIT_LOG" <<'EOF'
{"ts":1755800005,"event":"mint_result","address":"0xREDACTED000000000000000000000000000000","success":false,"detail":"reverted: MintQuantityExceedsMaxSupply","trigger_to_dispatch_ms":5,"send_to_ack_ms":142,"dispatch_to_inclusion_ms":1050,"prepare_age_ms":31}
EOF
RESULT=$("$FIND_SCRIPT" "$AUDIT_LOG" 1755800005)
check '[[ "$(echo "$RESULT" | jq -r ".success")" == "false" ]]' "extracts success=false correctly, still without crashing"

echo
echo "=== a timed-out fire (dispatch_to_inclusion_ms is JSON null) ==="
cat > "$AUDIT_LOG" <<'EOF'
{"ts":1755800010,"event":"mint_result","address":"0xREDACTED000000000000000000000000000000","success":false,"detail":"timed out waiting for inclusion","trigger_to_dispatch_ms":6,"send_to_ack_ms":150,"dispatch_to_inclusion_ms":null,"prepare_age_ms":32}
EOF
RESULT=$("$FIND_SCRIPT" "$AUDIT_LOG" 1755800010)
check '[[ "$(echo "$RESULT" | jq -r ".dispatch_to_inclusion_ms")" == "null" ]]' "a null dispatch_to_inclusion_ms (TimedOut outcome) round-trips as JSON null, not a crash"

echo
echo "=== no matching record yet (arm just happened, mint_result not written) ==="
cat > "$AUDIT_LOG" <<'EOF'
{"ts":1755800020,"event":"arm","address":null}
EOF
RESULT=$("$FIND_SCRIPT" "$AUDIT_LOG" 1755800020)
check '[[ -z "$RESULT" ]]' "prints nothing when no mint_result matches yet, rather than crashing"

echo
echo "=== multiple wallets firing per arm — takes the most recent one ==="
cat > "$AUDIT_LOG" <<'EOF'
{"ts":1755800030,"event":"mint_result","address":"0xWALLET1","success":true,"detail":"confirmed","trigger_to_dispatch_ms":4,"send_to_ack_ms":100,"dispatch_to_inclusion_ms":900,"prepare_age_ms":30}
{"ts":1755800031,"event":"mint_result","address":"0xWALLET2","success":true,"detail":"confirmed","trigger_to_dispatch_ms":4,"send_to_ack_ms":110,"dispatch_to_inclusion_ms":950,"prepare_age_ms":30}
{"ts":1755800032,"event":"mint_result","address":"0xWALLET3","success":true,"detail":"confirmed","trigger_to_dispatch_ms":4,"send_to_ack_ms":120,"dispatch_to_inclusion_ms":1000,"prepare_age_ms":30}
EOF
RESULT=$("$FIND_SCRIPT" "$AUDIT_LOG" 1755800030)
check '[[ "$(echo "$RESULT" | jq -r ".send_to_ack_ms")" == "120" ]]' "with multiple wallets firing per arm, takes the LAST matching record (existing, documented single-wallet-methodology assumption — unchanged by this fix)"

if [[ "$FAILURES" -eq 0 ]]; then
  echo
  echo "ALL TESTS PASSED"
  exit 0
else
  echo
  echo "$FAILURES TEST(S) FAILED"
  exit 1
fi
