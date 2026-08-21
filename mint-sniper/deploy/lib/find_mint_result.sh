#!/usr/bin/env bash
# STEP 15e FOLLOW-UP — real crash found live on fire 1/15 of an actual
# n=15 run, with the underlying mint pipeline itself confirmed correct
# (journalctl showed all wallets firing and confirming via the real WS
# PUSH path, method="push", real timing values logged). The bug was in
# how run-benchmark.sh read the result back out of audit.log:
#
#   jq: error (at <stdin>:1): Cannot index string with string "success"
#
# `AuditRecord` (audit.rs) serializes its `detail` field with
# `#[serde(flatten)]` — the MintResult object's own keys (success,
# send_to_ack_ms, dispatch_to_inclusion_ms, and a SEPARATE,
# differently-shaped `detail` STRING field carrying a human-readable
# message like "confirmed") land directly on the TOP-LEVEL audit
# record, not nested under a `"detail": {...}` wrapper the way the
# Rust field name suggests. A real audit.log mint_result line looks
# like:
#   {"ts":1755800001,"event":"mint_result","address":"0x...",
#    "success":true,"detail":"confirmed","trigger_to_dispatch_ms":4,
#    "send_to_ack_ms":139,"dispatch_to_inclusion_ms":978,
#    "prepare_age_ms":30}
# — reconstructed directly from audit.rs's own struct + the
# MintResult match arm's json!({...}), not guessed. `.detail.success`
# was indexing the top-level `detail` STRING with "success", never an
# object.
#
# Extracted into its own file (same reasoning as
# deploy/lib/swap_config_to_testnet.py and
# deploy/lib/check_wallet_balances.py before it) so this exact logic is
# unit-testable against a real-shaped fixture
# (deploy/tests/test-find-mint-result.sh) without any drift risk
# between what's tested and what run-benchmark.sh actually runs.
#
# Usage: find_mint_result.sh <audit_log_path> <since_unix_ts>
# Prints a compact JSON object {"success":...,"send_to_ack_ms":...,
# "dispatch_to_inclusion_ms":...} for the most recent matching
# mint_result record, or nothing (exit 0) if none found yet.

set -euo pipefail

AUDIT_LOG="$1"
SINCE="$2"

jq -c --argjson since "$SINCE" \
  'select(.event == "mint_result" and .ts >= $since) | {success, send_to_ack_ms, dispatch_to_inclusion_ms}' \
  "$AUDIT_LOG" 2>/dev/null | tail -1
