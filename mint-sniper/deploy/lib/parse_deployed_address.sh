#!/usr/bin/env bash
# STEP 26 — real bug found live on the VPS: benchmark-token.sh's redeploy
# mode used to grep `forge create`'s ENTIRE stdout for the first
# `0x[a-fA-F0-9]{40}` match. `forge create`'s real output prints
# "Deployer: 0x..." BEFORE "Deployed to: 0x..." (confirmed directly
# against Foundry's own documented output format, not assumed) — so that
# grep captured the DEPLOYER's address, not the deployed contract's.
#
# Confirmed live, not just theorized: two real forge-create + cast-send
# transaction pairs from an actual redeploy run, both status: 1. The
# forge-create transaction's own receipt.contractAddress (ground truth,
# read directly from the chain — never trust a script's own stdout
# summary for this) showed the real deployed contract at one address;
# the misparsed $NFT_CONTRACT variable — reused as the target for BOTH
# setMaxSupply and updatePublicDrop — was the DEPLOYER's own EOA. Both
# calls silently "succeeded" as no-op self-sends (an EOA has no code to
# revert against), leaving the real contract completely unconfigured.
# getPublicDrop against the real contract confirmed it: all-zero
# PublicDrop, never actually set.
#
# Extracted into its own file (same reasoning as
# deploy/lib/find_mint_result.sh and deploy/lib/swap_config_to_testnet.py
# before it) so this exact parsing logic is unit-testable against a
# real-shaped forge-create output fixture
# (deploy/tests/test-parse-deployed-address.sh), without any drift risk
# between what's tested and what benchmark-token.sh actually runs.
#
# Usage: forge create ... | parse_deployed_address.sh
#        (or: parse_deployed_address.sh < captured_output.txt)
# Prints the deployed contract address on stdout, or nothing (exit 1) if
# no "Deployed to: 0x..." line is found.

set -euo pipefail

ADDR=$(grep -oE 'Deployed to: 0x[a-fA-F0-9]{40}' | grep -oE '0x[a-fA-F0-9]{40}' | head -1 || true)

if [[ -z "$ADDR" ]]; then
  exit 1
fi

echo "$ADDR"
