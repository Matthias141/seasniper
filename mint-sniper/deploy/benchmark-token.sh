#!/usr/bin/env bash
# STEP 15c — check whether the step 14b Robinhood Chain testnet benchmark
# token is still live, and redeploy a fresh one if not. Same handoff
# pattern as every other deploy/*.sh script in this directory: the
# OPERATOR runs this on the real VPS (or any machine with real internet
# access — this genuinely needs GitHub + soliditylang.org reachable,
# neither of which the coding sandbox that authored this had), this
# session never does.
#
# Why this exists: step 14b's original benchmark token
# (0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9) was deployed "live for 7
# days from deployment" — by the time a real VPS exists to run step 15's
# benchmark, it may well have expired. This script tells you which, and
# fixes it if needed.
#
# Usage:
#   ./benchmark-token.sh check <nft_contract_address>
#   ./benchmark-token.sh redeploy
#
# 'redeploy' requires:
#   RPC_URL       - Robinhood Chain testnet HTTP RPC (your own config.toml's
#                    http_rpc_urls[0] if it's already pointed at testnet, or
#                    see config.example.toml's Robinhood Chain section for
#                    the wss://robinhood-testnet.g.alchemy.com/v2/YOUR_KEY
#                    pattern — this script wants the https:// form)
#   DEPLOYER_PK   - a private key (0x-prefixed) for a wallet holding
#                    Robinhood Chain TESTNET ETH from the faucet
#                    (https://faucet.testnet.chain.robinhood.com). Fine to
#                    reuse one of the bot's own configured sniper wallets
#                    (e.g. mint-sniper.env's SNIPER_PK_1) — it's testnet
#                    funds, separate from real mainnet balances, same
#                    prerequisite step 15e's benchmark script itself notes.
#                    NEVER export a MAINNET key into this script's
#                    environment.
#
# Both 'check' and 'redeploy' need `cast`/`forge` (Foundry) on PATH.
# THIS SESSION NEVER GOT FOUNDRY WORKING AT ALL — foundryup's GitHub
# release fetch returned 403 from this sandbox's scoped/proxied network
# access (see CLAUDE.md's step 14b section). That was a SANDBOX-specific
# block, not a Foundry-specific one — a real VPS with normal outbound
# internet access should install it the standard way with no special
# workaround needed:
#   curl -L https://foundry.paradigm.xyz | bash
#   foundryup
# If that ALSO 403s on the real VPS for some reason (a corporate/cloud
# egress policy, unlikely but not impossible), the standalone binary
# release on GitHub (https://github.com/foundry-rs/foundry/releases) can
# be downloaded and extracted manually as a fallback — not scripted here
# since it wasn't needed in practice; the sandbox's block was a scoped-
# API-token limitation this session's own network config imposed, not
# something expected to recur on a real machine with a real network path.

set -euo pipefail

MODE="${1:-}"
SEADROP_SINGLETON="0x00005EA00Ac477B1030CE78506496e8C2dE24bf5"  # SEADROP_1_0_MAINNET in seadrop.rs — same CREATE2 address on every EVM chain it's deployed to, confirmed working on Robinhood Chain testnet in step 14b
SEADROP_REPO="https://github.com/ProjectOpenSea/seadrop.git"
WORKDIR="${SEADROP_WORKDIR:-$HOME/.mint-sniper-seadrop-checkout}"

case "$MODE" in
  check)
    NFT_CONTRACT="${2:-}"
    if [[ -z "$NFT_CONTRACT" ]]; then
      echo "usage: $0 check <nft_contract_address>" >&2
      exit 1
    fi
    RPC_URL="${RPC_URL:?set RPC_URL to a Robinhood Chain testnet HTTP RPC first}"
    if ! command -v cast &>/dev/null; then
      echo "cast (Foundry) not found — see this script's header comment for install instructions" >&2
      exit 1
    fi

    echo "==> calling getPublicDrop($NFT_CONTRACT) on the SeaDrop singleton"
    # Struct order confirmed directly against seadrop's own
    # SeaDropStructs.sol (not assumed): mintPrice(uint80),
    # startTime(uint48), endTime(uint48), maxTotalMintableByWallet(uint16),
    # feeBps(uint16), restrictFeeRecipients(bool) — same decode this
    # codebase's own seadrop.rs::fetch_public_drop uses.
    RESULT=$(cast call "$SEADROP_SINGLETON" \
      "getPublicDrop(address)(uint80,uint48,uint48,uint16,uint16,bool)" \
      "$NFT_CONTRACT" --rpc-url "$RPC_URL")
    echo "$RESULT"

    # STEP 15c FOLLOW-UP — a real bug found live on the VPS: cast's
    # DEFAULT text output annotates any integer it judges "large" with a
    # human-readable bracket, e.g. a real endTime line looked like
    # `1787557476 [1.787e9]`, not the bare `1787557476` this script
    # originally assumed. Feeding that whole string into bash's (( ))
    # arithmetic below fails outright on the bracket — confirmed the
    # underlying RPC call and value were correct both times, this was
    # purely a parsing bug. Fixed by extracting just the leading digit
    # run with grep, deliberately NOT reaching for `cast call --json` +
    # jq instead: jq is not installed by default on a stock Ubuntu VPS
    # (confirmed live — nothing installed it the night this bug was
    # found), and grep/sed/tr ship with bash everywhere, so this has one
    # fewer dependency to go missing on a fresh box. `grep -oE '[0-9]+'`
    # is intentionally NOT anchored with `^` — it matches the first
    # digit run wherever it starts, so it doesn't care whether cast adds
    # leading whitespace, and it degrades gracefully (still correct) if
    # a future cast version ever drops the bracket annotation entirely.
    END_TIME=$(echo "$RESULT" | sed -n '3p' | grep -oE '[0-9]+' | head -1)
    NOW=$(date +%s)

    if [[ -z "$END_TIME" || "$END_TIME" == "0" ]]; then
      echo
      echo "==> endTime is 0 or unreadable — this nft_contract likely has no"
      echo "    public drop configured on this SeaDrop singleton at all (never"
      echo "    deployed, or deployed against a different seadrop_address)."
      echo "    Run: $0 redeploy"
      exit 1
    fi

    if (( END_TIME > NOW )); then
      REMAINING=$(( (END_TIME - NOW) / 3600 ))
      echo
      echo "==> STILL LIVE. endTime=$END_TIME (now=$NOW), ~${REMAINING}h remaining."
      echo "    Use this contract address for step 15e's benchmark:"
      echo "      $NFT_CONTRACT"
    else
      EXPIRED_HOURS=$(( (NOW - END_TIME) / 3600 ))
      echo
      echo "==> EXPIRED. endTime=$END_TIME (now=$NOW), expired ~${EXPIRED_HOURS}h ago."
      echo "    Run: $0 redeploy"
      exit 1
    fi
    ;;

  redeploy)
    RPC_URL="${RPC_URL:?set RPC_URL to a Robinhood Chain testnet HTTP RPC first}"
    DEPLOYER_PK="${DEPLOYER_PK:?set DEPLOYER_PK to a Robinhood Chain TESTNET-funded wallet's private key first — see this script's header comment}"
    if ! command -v forge &>/dev/null || ! command -v cast &>/dev/null; then
      echo "forge/cast (Foundry) not found — see this script's header comment for install instructions" >&2
      exit 1
    fi

    if [[ ! -d "$WORKDIR/.git" ]]; then
      echo "==> cloning $SEADROP_REPO (same repo step 14b's original artifact came from)"
      git clone --recurse-submodules "$SEADROP_REPO" "$WORKDIR"
    else
      echo "==> reusing existing checkout at $WORKDIR"
    fi
    cd "$WORKDIR"

    echo "==> forge build"
    forge build

    DEPLOYER_ADDR=$(cast wallet address --private-key "$DEPLOYER_PK")
    echo "==> deploying a fresh ERC721SeaDrop as $DEPLOYER_ADDR"
    # Constructor confirmed directly against the compiled artifact's ABI
    # (out/ERC721SeaDrop.sol/ERC721SeaDrop.json): (name, symbol,
    # address[] allowedSeaDrop) — same shape step 14b's web3.py deploy
    # used, just via forge this time since a real VPS has normal GitHub
    # access to actually install Foundry. NOT verified against a live
    # `forge` binary from this sandbox (Foundry never installed here —
    # see this script's header) — `forge create` broadcasts by default in
    # every Foundry version this was checked against in documentation,
    # so `--broadcast` below should be a no-op flag at worst; if your
    # installed version rejects it as unrecognized, just drop it and
    # re-run.
    DEPLOY_OUT=$(forge create src/ERC721SeaDrop.sol:ERC721SeaDrop \
      --rpc-url "$RPC_URL" \
      --private-key "$DEPLOYER_PK" \
      --broadcast \
      --constructor-args "mint-sniper-bench" "MSB" "[$SEADROP_SINGLETON]")
    echo "$DEPLOY_OUT"
    NFT_CONTRACT=$(echo "$DEPLOY_OUT" | grep -oE '0x[a-fA-F0-9]{40}' | head -1)
    if [[ -z "$NFT_CONTRACT" ]]; then
      echo "could not parse the deployed contract address out of forge's output above — check it manually" >&2
      exit 1
    fi
    echo "==> deployed at $NFT_CONTRACT"

    echo "==> setMaxSupply(1000) — comfortably above any planned benchmark attempt count"
    cast send "$NFT_CONTRACT" "setMaxSupply(uint256)" 1000 \
      --rpc-url "$RPC_URL" --private-key "$DEPLOYER_PK"

    NOW=$(date +%s)
    START_TIME=$(( NOW - 60 ))          # already live by the time this returns
    END_TIME=$(( NOW + 7 * 24 * 3600 )) # live for 7 days, same as step 14b's original

    echo "==> updatePublicDrop: free mint, unrestricted, maxTotalMintableByWallet=65535, live now for 7 days"
    # Same config as step 14b's original benchmark token, on purpose —
    # keeps this redeploy comparable to the original if it's ever needed
    # for a re-check: mintPrice=0, restrictFeeRecipients=false,
    # maxTotalMintableByWallet=65535 (uint16 max), feeBps=0.
    cast send "$NFT_CONTRACT" \
      "updatePublicDrop(address,(uint80,uint48,uint48,uint16,uint16,bool))" \
      "$SEADROP_SINGLETON" \
      "(0,$START_TIME,$END_TIME,65535,0,false)" \
      --rpc-url "$RPC_URL" --private-key "$DEPLOYER_PK"

    echo
    echo "==> done. New benchmark token: $NFT_CONTRACT"
    echo "    Update this in CLAUDE.md's step 14/15 sections (superseding the"
    echo "    expired address, same convention as every other superseded"
    echo "    number in this project) and in config.toml for step 15e:"
    echo "      mint_mode = \"seadrop\""
    echo "      nft_contract = \"$NFT_CONTRACT\""
    echo "      fee_recipient = \"0x0000a26b00c1F0DF003000390027140000fAa719\"  # OpenSea's official recipient"
    echo "      quantity_per_wallet = 1"
    ;;

  *)
    echo "usage: $0 check <nft_contract_address> | redeploy" >&2
    exit 1
    ;;
esac
