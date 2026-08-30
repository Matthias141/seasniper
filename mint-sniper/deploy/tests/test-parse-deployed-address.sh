#!/usr/bin/env bash
# STEP 26 — regression test for deploy/lib/parse_deployed_address.sh
# against a real-shaped `forge create` output fixture (the exact line
# order — "Deployer:" before "Deployed to:" — confirmed directly against
# Foundry's own documented output format, not guessed), reproducing the
# real bug found live: benchmark-token.sh's old plain
# `grep -oE '0x[a-fA-F0-9]{40}' | head -1` captured the DEPLOYER's
# address instead of the deployed contract's, because it matched
# whichever 40-hex-char string appeared first in forge's output —
# and "Deployer:" prints first. That misparsed address was then reused
# as the target for both setMaxSupply and updatePublicDrop, so neither
# call ever reached the real contract — confirmed on two real
# transaction pairs, both a silent no-op self-send to the deployer's own
# EOA (status: 1, since an EOA has no code to revert against).
# Run manually with: ./deploy/tests/test-parse-deployed-address.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PARSE_SCRIPT="$SCRIPT_DIR/../lib/parse_deployed_address.sh"
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

# The real addresses from tonight's actual redeploy run (step 26) — the
# deployer EOA and the real deployed contract, confirmed directly via
# eth_getTransactionReceipt + eth_getCode against Robinhood Chain
# testnet, not invented.
DEPLOYER_ADDR="0xb052c60a83bd79170a3ca470e4b1618ebdf28a10"
CONTRACT_ADDR="0x5f51dd4a6198b656e4820ca96ac1f927c722711c"

echo "=== real-shaped forge create output — the exact bug scenario ==="
echo "    (Deployer: prints BEFORE Deployed to:, per Foundry's real format —"
echo "     a naive first-match grep captures the wrong one)"
OUTPUT="No files changed, compilation skipped
Deployer: $DEPLOYER_ADDR
Deployed to: $CONTRACT_ADDR
Transaction hash: 0xb12b70ce297b7f28e31d358bd21b6ae9bf8a1ec54517844341ad8a2f3d536c1a"

RESULT=$(echo "$OUTPUT" | "$PARSE_SCRIPT")
check '[[ "$RESULT" == "$CONTRACT_ADDR" ]]' "extracts the Deployed-to address, not the Deployer's (the exact live bug)"
check '[[ "$RESULT" != "$DEPLOYER_ADDR" ]]' "does not accidentally extract the deployer's own address"

echo
echo "=== with a preceding compiler-run banner and trailing gas/verify noise ==="
OUTPUT="[⠊] Compiling...
No files changed, compilation skipped
Compiler run successful!
Deployer: $DEPLOYER_ADDR
Deployed to: $CONTRACT_ADDR
Transaction hash: 0xb12b70ce297b7f28e31d358bd21b6ae9bf8a1ec54517844341ad8a2f3d536c1a
Gas used: 4712869
Gas price: 10000000"

RESULT=$(echo "$OUTPUT" | "$PARSE_SCRIPT")
check '[[ "$RESULT" == "$CONTRACT_ADDR" ]]' "still correct with realistic surrounding forge output"

echo
echo "=== no 'Deployed to:' line at all (a failed/reverted deployment) ==="
OUTPUT="Deployer: $DEPLOYER_ADDR
Error: Failed to deploy contract"
set +e
RESULT=$(echo "$OUTPUT" | "$PARSE_SCRIPT")
CODE=$?
set -e
check '[[ "$CODE" -ne 0 ]]' "exits non-zero when no 'Deployed to:' line is found, rather than silently returning the deployer's address"
check '[[ -z "$RESULT" ]]' "prints nothing on failure"

if [[ "$FAILURES" -eq 0 ]]; then
  echo
  echo "ALL TESTS PASSED"
  exit 0
else
  echo
  echo "$FAILURES TEST(S) FAILED"
  exit 1
fi
