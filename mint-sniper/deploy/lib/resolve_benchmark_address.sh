#!/usr/bin/env bash
# STEP 27 — resolves which benchmark NFT contract address run-benchmark.sh's
# pre-flight "confirming the benchmark token is live" check should target.
#
# Real bug this fixes: run-benchmark.sh's BENCHMARK_CHECK_ADDR had exactly
# ONE hardcoded default -- step 14b's original benchmark token
# (0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9) -- with no mechanism to pick
# up a later `benchmark-token.sh redeploy`'s fresh address automatically,
# and BENCHMARK_CHECK_ADDR isn't even mentioned in run-benchmark.sh's own
# "Usage:" example (only in the Prerequisites section below it). An
# operator following the documented usage literally, after step 26's
# redeploy replaced the token with a new address
# (0x118fafd8511a04Df686e848425253c838B3a1a94), would silently re-check the
# STALE original address every run -- with no indication anything was
# wrong until the check failed, which then hit a SEPARATE silent-exit bug
# in run-benchmark.sh itself (see that script's own step 27 comment on the
# CHECK_OUTPUT capture).
#
# Precedence, highest to lowest:
#   1. BENCHMARK_CHECK_ADDR env var, if the operator set it explicitly --
#      a deliberate override always wins over any auto-discovery.
#   2. The state file benchmark-token.sh's own `redeploy` subcommand now
#      writes on success (deploy/.benchmark-token-address, gitignored --
#      it's operator-VPS runtime state, not repo content). This is the
#      "own discovery logic" that was missing before: the last real
#      redeploy's address, read back automatically, with no manual
#      copy-paste step for the operator to remember on the next run.
#   3. The original step 14b hardcoded address, unchanged -- last-resort
#      fallback for a fresh checkout that has never run `redeploy` at all.
#
# Usage: resolve_benchmark_address.sh <state_file_path> [env_value]
#   state_file_path - path to deploy/.benchmark-token-address (may not exist)
#   env_value        - the current value of $BENCHMARK_CHECK_ADDR, or empty
# Prints "<address> <source>" on stdout, source is one of: env, state-file,
# hardcoded-default. e.g.:
#   0x118fafd8511a04Df686e848425253c838B3a1a94 state-file

set -euo pipefail

STATE_FILE="${1:?usage: resolve_benchmark_address.sh <state_file_path> [env_value]}"
ENV_VALUE="${2:-}"
HARDCODED_DEFAULT="0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9"

if [[ -n "$ENV_VALUE" ]]; then
  echo "$ENV_VALUE env"
  exit 0
fi

if [[ -f "$STATE_FILE" ]]; then
  STATE_ADDR=$(grep -oE '0x[a-fA-F0-9]{40}' "$STATE_FILE" | head -1 || true)
  if [[ -n "$STATE_ADDR" ]]; then
    echo "$STATE_ADDR state-file"
    exit 0
  fi
fi

echo "$HARDCODED_DEFAULT hardcoded-default"
