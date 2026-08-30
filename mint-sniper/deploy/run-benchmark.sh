#!/usr/bin/env bash
# STEP 15e — fires the bot n=15+ times against the step 15c benchmark
# token and prints a clean p50/p90 summary, same methodology as step
# 14b (send->ack and dispatch->inclusion, both from real mint_result
# events, not simulated). Same handoff pattern as every deploy/*.sh
# script: the OPERATOR runs this on the real VPS, this session never
# does.
#
# STEP 15e FOLLOW-UP — confirmed live on the real VPS: the original
# version of this script assumed config.toml was ALREADY pointed at
# Robinhood Chain testnet before running, but nothing actually swapped
# it there — it fired straight against whatever network config.toml
# already had (mainnet, on the operator's real box), and the pre-flight
# balance check reported 0.000000000000000000 on every wallet (real
# testnet-funded wallets, wrong network) without stopping. This version
# does the swap itself: backs up config.toml, points it at testnet +
# the benchmark contract, restarts the service, waits for it to come
# back healthy, fires, THEN restores the original config.toml, restarts
# again, and verifies the restore byte-for-byte before reporting
# success. This whole cycle is wrapped in a trap so an interrupted or
# crashed run still restores config.toml — never leaves the bot pointed
# at testnet, or at a testnet contract with real mainnet wallets still
# configured. See CLAUDE.md's step 15e section for the fuller writeup
# of the live incident this responds to.
#
# THIS SCRIPT MUST RUN AS ROOT (sudo ./run-benchmark.sh) — it restarts
# a systemd service twice, which needs root regardless of who invokes
# it, and it edits config.toml under the mint-sniper system user's
# ownership (same pattern as deploy.sh: root overall, chown back to
# mint-sniper after any file write).
#
# PREREQUISITES — read before running, some need your own judgment,
# flagged explicitly:
#
#   1. Environment variables (no defaults with placeholder keys — this
#      script refuses to run without them, on purpose):
#        TESTNET_WS_RPC_URL   - e.g. wss://robinhood-testnet.g.alchemy.com/v2/YOUR_KEY
#        TESTNET_HTTP_RPC_URL - e.g. https://robinhood-testnet.g.alchemy.com/v2/YOUR_KEY
#      Optional:
#        BENCHMARK_CHECK_ADDR - STEP 27: usually don't need to set this by
#                                hand anymore. Auto-discovered via
#                                deploy/lib/resolve_benchmark_address.sh:
#                                this env var (if set) > the state file
#                                `benchmark-token.sh redeploy` now writes
#                                automatically on success
#                                (deploy/.benchmark-token-address) > step
#                                14b's original hardcoded token as a last-
#                                resort fallback. Set this explicitly only
#                                to deliberately override both of those.
#        MIN_BALANCE_ETH      - default 0.01, the per-wallet threshold
#                                the pre-flight balance gate enforces
#                                (mint price is 0 on the benchmark drop
#                                — this only needs to cover gas for
#                                15+ fires, 0.01 is a deliberately
#                                generous default, not a tight one)
#        SERVICE_NAME          - default "mint-sniper"
#        SERVICE_USER           - default "mint-sniper"
#
#   2. Exactly ONE [[wallets]] entry in config.toml going in — matches
#      step 14b's own methodology (sequential single-wallet fires,
#      comparable numbers). This script warns but does not refuse to
#      continue if that's not the case (see the wallet-count check
#      below) — your own judgment call if intentional.
#
#   3. That one configured wallet needs REAL Robinhood Chain TESTNET ETH
#      — get it from https://faucet.testnet.chain.robinhood.com. THIS
#      SCRIPT'S BALANCE GATE (below) catches an empty wallet before
#      firing, but confirming the faucet transfer actually landed in
#      the first place (rather than just "the faucet page said
#      success") is still your own judgment call — the gate can only
#      tell you the balance IS zero, not whether a pending faucet
#      transfer is about to arrive.
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
#   sudo TESTNET_WS_RPC_URL=... TESTNET_HTTP_RPC_URL=... \
#     ./run-benchmark.sh [N] [BOT_DIR]
#     N        - number of fires, default 15 (14b's own n)
#     BOT_DIR  - the bot's WorkingDirectory, default /opt/mint-sniper
#                (must contain config.toml, audit.log, .sniper-token)

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "run as root (sudo ./run-benchmark.sh) — see this script's header comment for why" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

N="${1:-15}"
BOT_DIR="${2:-/opt/mint-sniper}"
SERVICE_NAME="${SERVICE_NAME:-mint-sniper}"
SERVICE_USER="${SERVICE_USER:-mint-sniper}"
BOT_URL="${BOT_URL:-http://127.0.0.1:4117}"
INTER_FIRE_DELAY_SECS="${INTER_FIRE_DELAY_SECS:-3}"
FIRE_TIMEOUT_SECS="${FIRE_TIMEOUT_SECS:-40}"  # generous over inclusion_timeout_ms's 30000ms default
HEALTH_TIMEOUT_SECS="${HEALTH_TIMEOUT_SECS:-30}"
MIN_BALANCE_ETH="${MIN_BALANCE_ETH:-0.01}"

# STEP 27 — real bug found live: BENCHMARK_CHECK_ADDR used to have a single
# hardcoded default (step 14b's original benchmark token) with no mechanism
# to pick up a later `benchmark-token.sh redeploy`'s fresh address, and
# wasn't even mentioned in this script's own "Usage:" example above — an
# operator following that example literally would silently re-check a
# STALE address every run. Resolved via deploy/lib/resolve_benchmark_address.sh
# instead: env override > the state file `benchmark-token.sh redeploy` now
# writes on success > the original hardcoded fallback. See that script's own
# header comment for the full precedence and reasoning.
RESOLVED_BENCHMARK_ADDR=$("$SCRIPT_DIR/lib/resolve_benchmark_address.sh" \
  "$SCRIPT_DIR/.benchmark-token-address" "${BENCHMARK_CHECK_ADDR:-}")
BENCHMARK_CHECK_ADDR="${RESOLVED_BENCHMARK_ADDR%% *}"
BENCHMARK_CHECK_ADDR_SOURCE="${RESOLVED_BENCHMARK_ADDR#* }"

TESTNET_WS_RPC_URL="${TESTNET_WS_RPC_URL:?set TESTNET_WS_RPC_URL to your Robinhood Chain testnet wss:// Alchemy URL first}"
TESTNET_HTTP_RPC_URL="${TESTNET_HTTP_RPC_URL:?set TESTNET_HTTP_RPC_URL to your Robinhood Chain testnet https:// Alchemy URL first}"

# STEP 15c FOLLOW-UP — audited this script for the same class of bug
# benchmark-token.sh hit live (cast's bracket-annotated integers breaking
# naive bash arithmetic): confirmed this script never calls `cast` at
# all — its only numeric parsing is (a) bash arithmetic on values it
# generates itself and (b) `jq -r` reads from audit.log/api/status,
# which are plain serde_json output from the bot's own code — never
# cast's human-readable annotation, so no equivalent bug exists here.
for tool in curl jq python3; do
  if ! command -v "$tool" &>/dev/null; then
    echo "$tool is required and not on PATH — install it first (e.g. 'sudo apt-get install -y $tool' on Ubuntu)" >&2
    exit 1
  fi
done

CONFIG_PATH="$BOT_DIR/config.toml"
BACKUP_PATH="$BOT_DIR/config.toml.backup"
AUDIT_LOG="$BOT_DIR/audit.log"
TOKEN_FILE="$BOT_DIR/.sniper-token"

if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "no config.toml found at $CONFIG_PATH — run this from the bot's WorkingDirectory (/opt/mint-sniper), or pass it as \$2" >&2
  exit 1
fi
if [[ ! -f "$TOKEN_FILE" ]]; then
  echo "no .sniper-token found at $TOKEN_FILE" >&2
  exit 1
fi
# STEP 15e FOLLOW-UP — refuse to run if a backup already exists, rather
# than silently overwriting it. If a prior run crashed between backup
# and restore, config.toml.backup right now holds the operator's REAL
# original config — blindly `cp`-ing the CURRENT (possibly still
# testnet-swapped) config.toml over it here would permanently destroy
# the only copy of the real one. This is exactly the failure mode the
# whole restore-and-verify design exists to prevent; it must not be
# possible to trip it from the very first step.
if [[ -f "$BACKUP_PATH" ]]; then
  echo "$BACKUP_PATH already exists — refusing to proceed." >&2
  echo "This usually means a prior run of this script did not complete its restore" >&2
  echo "cleanly. Manually inspect both files RIGHT NOW:" >&2
  echo "  diff $BACKUP_PATH $CONFIG_PATH" >&2
  echo "If $BACKUP_PATH is your real original config, restore it yourself first:" >&2
  echo "  cp $BACKUP_PATH $CONFIG_PATH && systemctl restart $SERVICE_NAME" >&2
  echo "then remove $BACKUP_PATH before re-running this script." >&2
  exit 1
fi
API_TOKEN=$(cat "$TOKEN_FILE")
touch "$AUDIT_LOG"

wait_for_healthy() {
  local deadline=$(( $(date +%s) + HEALTH_TIMEOUT_SECS ))
  while (( $(date +%s) < deadline )); do
    if [[ "$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $API_TOKEN" "$BOT_URL/api/status" 2>/dev/null)" == "200" ]]; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# --- 1. back up the current config before touching anything ---
echo "==> backing up $CONFIG_PATH -> $BACKUP_PATH"
sudo -u "$SERVICE_USER" cp "$CONFIG_PATH" "$BACKUP_PATH"

RESULTS_FILE=$(mktemp)
RESTORE_VERIFIED=0

# STEP 15e FOLLOW-UP — the actual safety net. Wraps steps 2-6 (swap,
# restart, fire, restore, restart, verify): runs on ANY exit from this
# script — normal completion, a thrown error under `set -e`, or an
# operator Ctrl+C — so an interrupted run (same as happened live the
# night this was found) still restores config.toml instead of leaving
# the bot pointed at testnet with real mainnet wallets configured.
cleanup() {
  local exit_code=$?
  # `set -e` is unreliable inside trap handlers across bash versions —
  # this is the single most important command in the whole script, so
  # it does not lean on that: its own exit status is checked explicitly
  # rather than trusted to propagate.
  set +e
  echo
  echo "==> restoring $CONFIG_PATH from $BACKUP_PATH"
  sudo -u "$SERVICE_USER" cp "$BACKUP_PATH" "$CONFIG_PATH"
  local cp_status=$?

  # Step 7 — not optional, not best-effort: verify byte-for-byte before
  # ever reporting success.
  if [[ "$cp_status" -eq 0 ]] && cmp -s "$BACKUP_PATH" "$CONFIG_PATH"; then
    echo "==> restore verified: config.toml matches config.toml.backup byte-for-byte"
    systemctl restart "$SERVICE_NAME"
    if wait_for_healthy; then
      echo "==> service restarted and healthy on the restored config"
      RESTORE_VERIFIED=1
      rm -f "$BACKUP_PATH"
    else
      echo "==> !!! config.toml is restored correctly, but the service did NOT come back" >&2
      echo "    healthy within ${HEALTH_TIMEOUT_SECS}s. Check manually:" >&2
      echo "      systemctl status $SERVICE_NAME" >&2
      echo "      journalctl -u $SERVICE_NAME -n 100 --no-pager" >&2
      echo "    NOT deleting $BACKUP_PATH — keep it until you've confirmed this yourself." >&2
    fi
  else
    echo "==> !!! RESTORE FAILED — $CONFIG_PATH does NOT match $BACKUP_PATH after copy !!!" >&2
    echo "    DO NOT TRUST THE BOT'S CURRENT CONFIG. Manually run:" >&2
    echo "      diff $BACKUP_PATH $CONFIG_PATH" >&2
    echo "    and fix this before arming the bot for anything real." >&2
  fi

  rm -f "$RESULTS_FILE" 2>/dev/null || true

  if [[ "$exit_code" -ne 0 ]]; then
    echo "==> exiting non-zero (code $exit_code) — see messages above for what happened." >&2
  fi
  if [[ "$RESTORE_VERIFIED" -ne 1 ]]; then
    echo "==> exiting WITHOUT a confirmed-good restore — see the !!! messages above, this needs manual attention." >&2
    exit 1
  fi
  exit "$exit_code"
}
trap cleanup EXIT

# --- 2. confirm the benchmark token and swap config.toml to testnet ---
echo
echo "==> using benchmark token address: $BENCHMARK_CHECK_ADDR (source: $BENCHMARK_CHECK_ADDR_SOURCE)"
echo "==> confirming the benchmark token is live"
# STEP 27 — real silent-failure bug found live: benchmark-token.sh's own
# diagnostic text (STILL LIVE / EXPIRED / endTime-unreadable, WHY the check
# failed) used to print on stdout, which this line captures into a
# variable. Under `set -euo pipefail`, a plain `VAR=$(cmd)` assignment
# propagates cmd's own exit status — so when benchmark-token.sh exited
# non-zero (e.g. EXPIRED), THIS SCRIPT aborted immediately at this exact
# line, before the `echo "$CHECK_OUTPUT"` below ever ran. The real reason
# WAS captured into CHECK_OUTPUT, it just never got printed anywhere —
# a completely silent, bare non-zero exit, with cleanup()'s own "see
# messages above" text pointing at messages that were never shown.
#
# Fixed two ways, not just one: (1) benchmark-token.sh's own diagnostics
# now go to stderr (see its step 27 comment), so they show up live on the
# terminal as they happen regardless of what happens to stdout here; (2)
# this script no longer lets `set -e` silently abort past this call at
# all — it explicitly disables -e for just this one command, captures the
# real exit status, and reports it plainly before deciding what to do
# next, the same "surface the real reason, don't swallow it" standard
# already applied to benchmark-token.sh's cast-bracket fix and the step 17
# WS-connect error wrapping.
set +e
CHECK_OUTPUT=$(RPC_URL="$TESTNET_HTTP_RPC_URL" "$SCRIPT_DIR/benchmark-token.sh" check "$BENCHMARK_CHECK_ADDR")
CHECK_STATUS=$?
set -e
if [[ "$CHECK_STATUS" -ne 0 ]]; then
  echo "==> benchmark-token.sh check exited $CHECK_STATUS — see the diagnostic output" >&2
  echo "    above (now printed to stderr, live, as it happened) for the real reason." >&2
fi
echo "$CHECK_OUTPUT"
NFT_CONTRACT=$(echo "$CHECK_OUTPUT" | grep '^BENCHMARK_NFT_CONTRACT=' | cut -d= -f2)
if [[ -z "$NFT_CONTRACT" ]]; then
  echo "could not confirm a live benchmark token against $BENCHMARK_CHECK_ADDR (source:" >&2
  echo "$BENCHMARK_CHECK_ADDR_SOURCE) — see the diagnostic output above for why. If" >&2
  echo "EXPIRED, run './benchmark-token.sh redeploy' — it now auto-records the fresh" >&2
  echo "address for this script to pick up next time, no BENCHMARK_CHECK_ADDR override" >&2
  echo "needed (see deploy/lib/resolve_benchmark_address.sh)." >&2
  exit 1
fi

echo
echo "==> swapping $CONFIG_PATH to Robinhood Chain testnet + this benchmark contract"
python3 "$SCRIPT_DIR/lib/swap_config_to_testnet.py" \
  "$CONFIG_PATH" "$TESTNET_WS_RPC_URL" "$TESTNET_HTTP_RPC_URL" "$NFT_CONTRACT"
chown "$SERVICE_USER:$SERVICE_USER" "$CONFIG_PATH"

# --- 3. restart and wait for real health, not a fixed sleep ---
echo "==> restarting $SERVICE_NAME on the swapped config"
systemctl restart "$SERVICE_NAME"
if ! wait_for_healthy; then
  echo "service did not come back healthy within ${HEALTH_TIMEOUT_SECS}s after the testnet swap — check:" >&2
  echo "  systemctl status $SERVICE_NAME" >&2
  echo "  journalctl -u $SERVICE_NAME -n 100 --no-pager" >&2
  echo "(config.toml will still be restored by this script's cleanup before it exits)" >&2
  exit 1
fi
echo "==> service healthy on testnet config"

# STEP 28 — real gap found while writing up a benchmark result: this
# script's config swap (swap_config_to_testnet.py) never touches
# race_mode/sequencer_http_url at all — it only rewrites the network/
# target fields it explicitly lists (ws_rpc_url, http_rpc_urls,
# mint_mode, nft_contract, fee_recipient, quantity_per_wallet,
# block_time_ms). Whatever race_mode/sequencer_http_url the config
# already had BEFORE the swap carries through unchanged, silently. A
# ~5.5x dispatch_to_inclusion improvement was observed between two
# benchmark runs, and the leading hypothesis was PR #9's sequencer-racing
# feature — but neither this script's own output NOR audit.log's
# persisted MintResult records say which submission path (sequencer vs.
# backup RPC) actually acked each fire (that detail only ever reaches
# `tracing::info!`/journalctl, per executor.rs's "using this URL's ack"
# log line — never the durable audit trail), so the cause could not be
# confirmed after the fact. Same class of bug as steps 26/27: a script
# silently blind to a newer feature. Fixed here by making this explicit
# and printed, so a FUTURE run's own console output is self-sufficient
# evidence — no more "which mode was this actually running under?"
# ambiguity. NOT fixed: forcing race_mode to a specific value here would
# change this script's behavior beyond what was asked and isn't needed —
# reporting what's actually configured is the real gap.
echo
echo "==> race_mode / sequencer_http_url as the bot is ACTUALLY running (GET /api/config,"
echo "    read post-swap-restart — this is what this run's numbers should be attributed to,"
echo "    not assumed from the swap script, which never touches either field):"
RUNTIME_CONFIG=$(curl -sS -H "Authorization: Bearer $API_TOKEN" "$BOT_URL/api/config")
echo "$RUNTIME_CONFIG" | jq '{race_mode, sequencer_http_url}'

# balance_poll_loop refreshes wallet balances every 15s (main.rs) — give
# it time to complete at least one real cycle against the NEW network
# before trusting balance_eth below; checking immediately after restart
# would still show stale/zero data from before the swap.
echo "==> waiting 20s for the first post-restart balance poll to land"
sleep 20

echo
echo "==> pre-flight: GET /api/status"
STATUS=$(curl -sS -H "Authorization: Bearer $API_TOKEN" "$BOT_URL/api/status")
echo "$STATUS" | jq .
WALLET_COUNT=$(echo "$STATUS" | jq '.wallets | length')
if [[ "$WALLET_COUNT" != "1" ]]; then
  echo
  echo "==> WARNING: $WALLET_COUNT wallets configured, not 1. See prerequisite #2"
  echo "    above — this script's per-attempt timing assumes a single"
  echo "    sequential wallet, same as step 14b's methodology. Continuing"
  echo "    anyway, but treat the results with that caveat if this wasn't"
  echo "    intentional."
fi
if [[ "$(echo "$STATUS" | jq -r '.armed')" == "true" ]]; then
  echo "bot is already armed — disarm it first (POST /api/abort) before starting a clean benchmark run" >&2
  exit 1
fi

# STEP 15e FOLLOW-UP — the pre-flight balance gate that was specified
# but never actually built. Hard stop, not a warning: this is exactly
# what would have caught the live incident (three wallets correctly
# reporting 0 ETH on the network the bot was ACTUALLY connected to)
# before firing, instead of proceeding anyway.
UNDERFUNDED=$(echo "$STATUS" | python3 "$SCRIPT_DIR/lib/check_wallet_balances.py" "$MIN_BALANCE_ETH")
if [[ -n "$UNDERFUNDED" ]]; then
  echo
  echo "==> STOPPING — the following wallet(s) are below MIN_BALANCE_ETH=$MIN_BALANCE_ETH ETH" >&2
  echo "    on Robinhood Chain testnet. Fund them from the faucet" >&2
  echo "    (https://faucet.testnet.chain.robinhood.com) and confirm the transfer" >&2
  echo "    actually landed before re-running:" >&2
  echo "$UNDERFUNDED" >&2
  exit 1
fi
echo "==> balance gate passed — all configured wallets have >= $MIN_BALANCE_ETH ETH on testnet"

# --- 4. the actual n=15+ fires ---
echo
echo "==> starting $N fires against $NFT_CONTRACT"
echo "    (config.toml's mint_mode=seadrop forces trigger_mode=timestamp"
echo "    with the drop's real, already-past startTime at boot, per"
echo "    main.rs — so each /api/arm below auto-fires within ~1s, no"
echo "    separate /api/trigger call needed.)"
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
  # offsets. STEP 15e FOLLOW-UP — the actual field extraction lives in
  # deploy/lib/find_mint_result.sh now, not inline here: see that
  # file's header for the real bug this fixes (AuditRecord's
  # #[serde(flatten)] means success/send_to_ack_ms/
  # dispatch_to_inclusion_ms are top-level fields in audit.log, not
  # nested under a "detail" object) and why it's a separate file
  # (independently testable, no drift risk between what's tested and
  # what actually runs).
  DEADLINE=$(( $(date +%s) + FIRE_TIMEOUT_SECS ))
  FOUND=""
  while (( $(date +%s) < DEADLINE )); do
    FOUND=$("$SCRIPT_DIR/lib/find_mint_result.sh" "$AUDIT_LOG" "$ARM_TS")
    if [[ -n "$FOUND" ]]; then
      break
    fi
    sleep 1
  done

  if [[ -z "$FOUND" ]]; then
    echo "   TIMED OUT after ${FIRE_TIMEOUT_SECS}s waiting for a mint_result — check journalctl -u $SERVICE_NAME for what happened" >&2
    curl -sS -o /dev/null -X POST -H "Authorization: Bearer $API_TOKEN" "$BOT_URL/api/abort" || true
    continue
  fi

  echo "$FOUND" >> "$RESULTS_FILE"
  SUCCESS=$(echo "$FOUND" | jq -r '.success')
  SEND_ACK=$(echo "$FOUND" | jq -r '.send_to_ack_ms')
  DISPATCH_INCL=$(echo "$FOUND" | jq -r '.dispatch_to_inclusion_ms')
  echo "   success=$SUCCESS send_to_ack_ms=$SEND_ACK dispatch_to_inclusion_ms=$DISPATCH_INCL"

  sleep "$INTER_FIRE_DELAY_SECS"
done

echo
echo "==> push vs. poll (step 15d cross-check, from this run's journal window):"
PUSH_COUNT=$(journalctl -u "$SERVICE_NAME" --since "-$(( N * (INTER_FIRE_DELAY_SECS + FIRE_TIMEOUT_SECS) ))s" 2>/dev/null \
  | grep -c "WS push path established" || true)
POLL_COUNT=$(journalctl -u "$SERVICE_NAME" --since "-$(( N * (INTER_FIRE_DELAY_SECS + FIRE_TIMEOUT_SECS) ))s" 2>/dev/null \
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
        # STEP 15e FOLLOW-UP — same bug as the jq extraction above:
        # AuditRecord flattens `detail` onto the top-level record
        # (audit.rs's `#[serde(flatten)] detail: serde_json::Value`),
        # so success/send_to_ack_ms/dispatch_to_inclusion_ms are
        # top-level keys, not nested under a "detail" object.
        rec = json.loads(line)
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

echo
echo "==> Paste the p50/p90 numbers above into CLAUDE.md's step 15f section,"
echo "    marking step 14b's HTTP-poll-only numbers as superseded (not"
echo "    deleted), same convention as every other superseded figure in"
echo "    this project. Paste this run's race_mode/sequencer_http_url"
echo "    values (printed earlier, before the fires started) alongside"
echo "    them too — attributing a result to sequencer racing without"
echo "    that confirmed is a guess, not a finding (step 28)."
echo
echo "==> STEP 28 — if you need to know which submission path (sequencer vs."
echo "    backup RPC) acked any INDIVIDUAL fire, that detail is not in"
echo "    audit.log (MintResult's persisted detail is just the tx hash on"
echo "    success) — it only ever reaches journalctl, via executor.rs's"
echo "    'using this URL's ack for send_to_ack_ms' info! line. Check"
echo "    journalctl -u $SERVICE_NAME while this run is still fresh if"
echo "    per-fire attribution matters, not after the fact."
echo
echo "==> If any individual dispatch_to_inclusion_ms is a clear outlier"
echo "    (many multiples of step 14b's measured ~227ms block time),"
echo "    investigate before trusting it at face value — it can mean real"
echo "    on-chain delay, or this bot's own RPC subscription lagging"
echo "    behind real inclusion, and those need very different responses."
echo "    Grab that fire's tx hash from the matching audit.log entry's"
echo "    \"detail\" field (the literal tx hash on a successful mint), then run:"
echo "      python3 \"$SCRIPT_DIR/lib/diagnose_inclusion_delay.py\" \\"
echo "        \$TESTNET_HTTP_RPC_URL <tx_hash> <dispatch_to_inclusion_ms>"
echo
echo "==> steps 5-7 (restore original config.toml, restart, verify) run next"
echo "    automatically via this script's cleanup trap — see output below."

# --- 5-7: restore, restart, verify — handled by the cleanup trap above,
# which runs unconditionally on exit, including this normal completion.
