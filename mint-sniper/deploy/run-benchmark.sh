#!/usr/bin/env bash
# STEP 15e — fires the bot n=15+ times against the step 15c benchmark
# token and prints a clean p50/p90 summary, same methodology as step
# 14b (send->ack and dispatch->inclusion, both from real mint_result
# events, not simulated) — but automated end to end instead of the
# manual per-fire commands step 17's live debugging session had to fall
# back on. Same handoff pattern as every deploy/*.sh script: the
# OPERATOR runs this on the real VPS, this session never does.
#
# PREREQUISITES — read before running, some need your own judgment,
# flagged explicitly:
#
#   1. config.toml on this VPS must already point at the confirmed-live
#      benchmark token from step 15c:
#        mint_mode = "seadrop"
#        nft_contract = "<address from step 15c>"
#        fee_recipient = "0x0000a26b00c1F0DF003000390027140000fAa719"
#        quantity_per_wallet = 1
#        ws_rpc_url / http_rpc_urls pointed at Robinhood Chain TESTNET
#        block_time_ms = 227   (step 14b's measured figure — re-measure
#                                 if you suspect it's drifted)
#      Exactly ONE [[wallets]] entry — matches step 14b's own
#      methodology (sequential single-wallet fires, comparable numbers).
#      Multiple wallets would fire in parallel per arm and muddy the
#      per-attempt timing this script reports.
#
#   2. That one configured wallet needs REAL Robinhood Chain TESTNET ETH
#      — get it from https://faucet.testnet.chain.robinhood.com. THIS
#      SCRIPT DOES NOT CHECK THIS FOR YOU AT ALL (the pre-flight below
#      only checks wallet COUNT, not balance) — confirming the faucet
#      transfer actually landed (rather than just "the faucet page said
#      success") is your own judgment call before spending 15+ real
#      testnet mints on a wallet that might run dry partway through. A
#      dry wallet fails loudly per-attempt (an obvious "insufficient
#      funds" in the fire's own error), not silently, but check first
#      rather than discovering it mid-run.
#
#   3. The bot service must already be running with this config loaded
#      (`sudo systemctl restart mint-sniper` after any config.toml edit
#      — config.toml is only read at boot, per Config::load's own
#      design, same as every other step in this project that's touched
#      it).
#
#   4. If config.toml's google_oauth_client_id is SET (step 10c identity
#      enabled), /api/arm requires a fresh X-Step-Up-Totp header per
#      request (auth.rs::require_step_up) — a real 6-digit code from
#      your authenticator app, valid ~30s and single-use. THIS SCRIPT
#      CANNOT AUTOMATE THAT LOOP — a human has to be present typing in a
#      fresh code before each of the 15+ arms, which defeats the point
#      of an unattended benchmark run. If identity is enabled on this
#      instance, either benchmark against a second, identity-disabled
#      instance/config, or accept that this script needs to be run
#      interactively with you supplying step-up codes on request (not
#      implemented here — flagged, not silently worked around).
#
# Usage:
#   ./run-benchmark.sh [N] [BOT_DIR]
#     N        - number of fires, default 15 (14b's own n)
#     BOT_DIR  - the bot's WorkingDirectory, default: current directory
#                (must contain audit.log and .sniper-token — run this
#                from /opt/mint-sniper, or pass that path explicitly)

set -euo pipefail

N="${1:-15}"
BOT_DIR="${2:-.}"
BOT_URL="${BOT_URL:-http://127.0.0.1:4117}"
INTER_FIRE_DELAY_SECS="${INTER_FIRE_DELAY_SECS:-3}"
FIRE_TIMEOUT_SECS="${FIRE_TIMEOUT_SECS:-40}"  # generous over inclusion_timeout_ms's 30000ms default

for tool in curl jq python3; do
  if ! command -v "$tool" &>/dev/null; then
    echo "$tool is required and not on PATH" >&2
    exit 1
  fi
done

AUDIT_LOG="$BOT_DIR/audit.log"
TOKEN_FILE="$BOT_DIR/.sniper-token"
if [[ ! -f "$TOKEN_FILE" ]]; then
  echo "no .sniper-token found at $TOKEN_FILE — run this from the bot's WorkingDirectory (/opt/mint-sniper), or pass it as \$2" >&2
  exit 1
fi
API_TOKEN=$(cat "$TOKEN_FILE")
touch "$AUDIT_LOG"  # so tail/wc below don't fail if nothing's been logged yet

echo "==> pre-flight: GET /api/status"
STATUS=$(curl -sS -H "Authorization: Bearer $API_TOKEN" "$BOT_URL/api/status")
echo "$STATUS" | jq .
WALLET_COUNT=$(echo "$STATUS" | jq '.wallets | length')
if [[ "$WALLET_COUNT" != "1" ]]; then
  echo
  echo "==> WARNING: $WALLET_COUNT wallets configured, not 1. See prerequisite #1"
  echo "    above — this script's per-attempt timing assumes a single"
  echo "    sequential wallet, same as step 14b's methodology. Continuing"
  echo "    anyway, but treat the results with that caveat if this wasn't"
  echo "    intentional."
fi
if [[ "$(echo "$STATUS" | jq -r '.armed')" == "true" ]]; then
  echo "bot is already armed — disarm it first (POST /api/abort) before starting a clean benchmark run" >&2
  exit 1
fi

RESULTS_FILE=$(mktemp)
trap 'rm -f "$RESULTS_FILE"' EXIT

echo
echo "==> starting $N fires against the configured seadrop nft_contract"
echo "    (config.toml's mint_mode=seadrop forces trigger_mode=timestamp"
echo "    with the drop's real, already-past startTime at boot, per"
echo "    main.rs — so each /api/arm below auto-fires within ~1s, no"
echo "    separate /api/trigger call needed. See CLAUDE.md step 15e note"
echo "    if this assumption doesn't hold for your config.)"
echo

for i in $(seq 1 "$N"); do
  ARM_TS=$(date +%s)
  echo "-- fire $i/$N --"
  RESP=$(curl -sS -o /dev/null -w '%{http_code}' -X POST \
    -H "Authorization: Bearer $API_TOKEN" \
    "$BOT_URL/api/arm")
  if [[ "$RESP" != "202" ]]; then
    echo "   /api/arm returned HTTP $RESP, expected 202 — skipping this attempt" >&2
    continue
  fi

  # Poll audit.log for a mint_result event timestamped at/after this
  # arm — audit.log is append-only JSON lines (audit.rs::append), so a
  # simple tail+jq filter on ts is sufficient without tracking byte
  # offsets.
  DEADLINE=$(( $(date +%s) + FIRE_TIMEOUT_SECS ))
  FOUND=""
  while (( $(date +%s) < DEADLINE )); do
    FOUND=$(jq -c --argjson since "$ARM_TS" \
      'select(.event == "mint_result" and .ts >= $since)' \
      "$AUDIT_LOG" 2>/dev/null | tail -1 || true)
    if [[ -n "$FOUND" ]]; then
      break
    fi
    sleep 1
  done

  if [[ -z "$FOUND" ]]; then
    echo "   TIMED OUT after ${FIRE_TIMEOUT_SECS}s waiting for a mint_result — check journalctl -u mint-sniper for what happened" >&2
    # Best-effort recovery so the loop doesn't wedge on a stuck arm.
    curl -sS -o /dev/null -X POST -H "Authorization: Bearer $API_TOKEN" "$BOT_URL/api/abort" || true
    continue
  fi

  echo "$FOUND" >> "$RESULTS_FILE"
  SUCCESS=$(echo "$FOUND" | jq -r '.detail.success')
  SEND_ACK=$(echo "$FOUND" | jq -r '.detail.send_to_ack_ms')
  DISPATCH_INCL=$(echo "$FOUND" | jq -r '.detail.dispatch_to_inclusion_ms')
  echo "   success=$SUCCESS send_to_ack_ms=$SEND_ACK dispatch_to_inclusion_ms=$DISPATCH_INCL"

  sleep "$INTER_FIRE_DELAY_SECS"
done

echo
echo "==> push vs. poll (step 15d cross-check, from this run's journal window):"
PUSH_COUNT=$(journalctl -u mint-sniper --since "-$(( N * (INTER_FIRE_DELAY_SECS + FIRE_TIMEOUT_SECS) ))s" 2>/dev/null \
  | grep -c "WS push path established" || true)
POLL_COUNT=$(journalctl -u mint-sniper --since "-$(( N * (INTER_FIRE_DELAY_SECS + FIRE_TIMEOUT_SECS) ))s" 2>/dev/null \
  | grep -c "WS push path unavailable" || true)
echo "    PUSH established: $PUSH_COUNT / POLL fallback: $POLL_COUNT (out of $N arms)"
if [[ "$POLL_COUNT" -gt 0 ]]; then
  echo "    NOTE: $POLL_COUNT arm(s) fell back to POLL — gap #11 may not be"
  echo "    fully closed on this box, or the WS connect hit its 5s ceiling"
  echo "    under load. The p50/p90 below still reports real numbers either"
  echo "    way, but mixing PUSH and POLL attempts in one run muddies the"
  echo "    comparison 15f wants — consider re-running if this count is"
  echo "    high relative to $N."
fi

echo
echo "==> summary (n=$(wc -l < "$RESULTS_FILE") successful fires out of $N attempted)"
python3 - "$RESULTS_FILE" <<'PYEOF'
import json, sys

path = sys.argv[1]
send_ack = []
dispatch_incl = []
successes = 0
total = 0

with open(path) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        total += 1
        rec = json.loads(line)["detail"]
        if rec.get("success"):
            successes += 1
        sa = rec.get("send_to_ack_ms")
        di = rec.get("dispatch_to_inclusion_ms")
        if sa is not None:
            send_ack.append(sa)
        if di is not None:
            dispatch_incl.append(di)

def pct(values, p):
    if not values:
        return None
    s = sorted(values)
    k = (len(s) - 1) * (p / 100)
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)

print(f"successes: {successes}/{total}")
for label, values in (("send_to_ack_ms", send_ack), ("dispatch_to_inclusion_ms", dispatch_incl)):
    p50 = pct(values, 50)
    p90 = pct(values, 90)
    if p50 is None:
        print(f"{label}: no data")
    else:
        print(f"{label}: p50={p50:.0f}ms p90={p90:.0f}ms (n={len(values)})")
PYEOF

trap - EXIT  # cancel cleanup — the raw results are worth keeping
echo
echo "==> raw per-fire results (not auto-deleted — rm it yourself when done):"
echo "    $RESULTS_FILE"
echo
echo "==> Paste the p50/p90 numbers above into CLAUDE.md's step 15f section,"
echo "    marking step 14b's HTTP-poll-only numbers as superseded (not"
echo "    deleted), same convention as every other superseded figure in"
echo "    this project."
