#!/usr/bin/env python3
"""STEP 15 FOLLOW-UP — investigates a slow dispatch_to_inclusion_ms
number, distinguishing two very different explanations that this
bot's own measurement (subscribe_blocks() PUSH detection, or the HTTP
poll fallback) cannot tell apart on its own:

  (a) the tx really did take many real chain blocks to get included
      (a sequencer/chain fact, true regardless of which RPC you ask), vs
  (b) the tx was included quickly on-chain, but THIS bot's subscribed
      RPC node was itself slow to learn about / propagate that new
      block to its subscribers, so `dispatch_to_inclusion_ms` measures
      node lag, not real inclusion time.

Public Robinhood Chain docs (docs.robinhood.com/chain) describe
first-come-first-served sequencing where "higher gas fees do not
confer priority" and mention a SEPARATE low-latency sequencer feed
(`wss://feed.testnet.chain.robinhood.com`) distinct from a standard
RPC provider's own node — consistent with (b) being a real, plausible
explanation on this specific chain, and with gas pricing being an
UNLIKELY explanation (fee level doesn't affect ordering here) despite
being the intuitive first guess on most other EVM chains. This script
checks both anyway, with real on-chain data, rather than assuming
either.

Usage:
  diagnose_inclusion_delay.py <rpc_url> <tx_hash> <dispatch_to_inclusion_ms>

  <dispatch_to_inclusion_ms> — copy this straight from the audit.log
  record for the fire you're investigating (now readable without a
  crash, per the run-benchmark.sh parsing fix). Used to estimate which
  block was live at the moment of dispatch, since the bot doesn't
  currently log an absolute dispatch wall-clock timestamp anywhere —
  only this duration.

No external dependencies beyond Python 3's stdlib (urllib) — this
needs to run on a fresh VPS with nothing else installed. The RPC I/O
(`rpc_call`/`get_block`) is kept separate from `analyze()`, the actual
decision logic, specifically so `analyze()` is unit-testable against
fixture blocks with no network at all — see
deploy/tests/test-diagnose-inclusion-delay.py.
"""
import json
import sys
import urllib.request


def rpc_call(rpc_url: str, method: str, params: list):
    payload = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(rpc_url, data=payload, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=15) as resp:
        body = json.loads(resp.read())
    if "error" in body:
        raise RuntimeError(f"{method} failed: {body['error']}")
    return body["result"]


def analyze(receipt: dict, get_block, dispatch_to_inclusion_ms: int) -> dict:
    """Pure decision logic, no network I/O of its own — `get_block` is an
    injected `int -> dict` callable (block number -> eth_getBlockByNumber
    result) so this is testable against an in-memory fixture. Returns a
    dict of everything the CLI output below prints, so a test can assert
    on exact values rather than parsing printed text.
    """
    inclusion_block_num = int(receipt["blockNumber"], 16)
    effective_gas_price = int(receipt["effectiveGasPrice"], 16)
    status_ok = int(receipt["status"], 16) == 1

    inclusion_block = get_block(inclusion_block_num)
    inclusion_ts = int(inclusion_block["timestamp"], 16)
    base_fee_at_inclusion = inclusion_block.get("baseFeePerGas")
    priority_paid = None
    if base_fee_at_inclusion is not None:
        base_fee_at_inclusion = int(base_fee_at_inclusion, 16)
        priority_paid = effective_gas_price - base_fee_at_inclusion

    approx_dispatch_ts = inclusion_ts - (dispatch_to_inclusion_ms / 1000)

    # Walk backward from the inclusion block until we find the last block
    # produced at or before the estimated dispatch instant. Bounded to a
    # generous 200 blocks back so a wildly wrong dispatch_to_inclusion_ms
    # input can't turn this into an unbounded RPC-hammering loop.
    block_num = inclusion_block_num
    block_ts = inclusion_ts
    steps = 0
    timestamps = [(block_num, block_ts)]
    while block_ts > approx_dispatch_ts and steps < 200:
        block_num -= 1
        if block_num < 0:
            break
        block = get_block(block_num)
        block_ts = int(block["timestamp"], 16)
        timestamps.append((block_num, block_ts))
        steps += 1

    dispatch_block_num = block_num
    blocks_elapsed = inclusion_block_num - dispatch_block_num

    # Real measured block time across the window walked, for a sanity
    # cross-check against step 14b's 227ms Robinhood testnet figure —
    # timestamps alone are only 1-second granular (same caveat 14b's own
    # measurement noted), so this is indicative over a short window, not
    # a precise re-measurement on its own.
    avg_block_time_ms = None
    if len(timestamps) >= 2:
        span_secs = timestamps[0][1] - timestamps[-1][1]
        span_blocks = timestamps[0][0] - timestamps[-1][0]
        if span_blocks > 0:
            avg_block_time_ms = (span_secs / span_blocks) * 1000

    return {
        "inclusion_block_num": inclusion_block_num,
        "status_ok": status_ok,
        "effective_gas_price": effective_gas_price,
        "base_fee_at_inclusion": base_fee_at_inclusion,
        "priority_paid": priority_paid,
        "dispatch_block_num": dispatch_block_num,
        "blocks_elapsed": blocks_elapsed,
        "avg_block_time_ms": avg_block_time_ms,
        # The actual verdict: <=2 blocks means real inclusion was fast
        # and the measured duration is a detection/node-lag artifact,
        # not genuine sequencer delay.
        "likely_node_lag_not_real_delay": blocks_elapsed <= 2,
    }


def print_report(result: dict) -> None:
    print(f"    included in block {result['inclusion_block_num']}, "
          f"status={'success' if result['status_ok'] else 'reverted'}")
    print(f"    effectiveGasPrice: {result['effective_gas_price']} wei "
          f"({result['effective_gas_price'] / 1e9:.4f} gwei)")
    if result["base_fee_at_inclusion"] is not None:
        print(f"    block baseFeePerGas at inclusion: {result['base_fee_at_inclusion']} wei "
              f"({result['base_fee_at_inclusion'] / 1e9:.4f} gwei)")
        print(f"    effective priority actually paid: {result['priority_paid']} wei "
              f"({result['priority_paid'] / 1e9:.4f} gwei)")
        if result["priority_paid"] <= 0:
            print("    NOTE: effectiveGasPrice <= baseFeePerGas at inclusion — the tx paid")
            print("    essentially zero priority fee. On most EVM chains this would be a real")
            print("    red flag for underpricing; per Robinhood Chain's own docs (FCFS")
            print("    sequencing, gas fee doesn't affect order), this may simply be normal")
            print("    here rather than causal for any inclusion delay — worth noting, not")
            print("    assuming either way without corroborating evidence.")

    print(f"\n==> RESULT")
    print(f"    dispatch (estimated):  block ~{result['dispatch_block_num']}")
    print(f"    inclusion (confirmed): block {result['inclusion_block_num']}")
    print(f"    blocks elapsed: {result['blocks_elapsed']}")
    if result["avg_block_time_ms"] is not None:
        print(f"    avg block time over this window: {result['avg_block_time_ms']:.0f}ms "
              f"(1-second timestamp granularity — indicative, not precise)")

    print()
    if result["likely_node_lag_not_real_delay"]:
        print("    ==> Inclusion itself was fast (<=2 blocks). The measured")
        print("        dispatch_to_inclusion_ms is NOT real on-chain inclusion delay —")
        print("        it's this bot's subscribed RPC node being slow to learn about / push")
        print("        the new block to its subscriber, i.e. a node/subscription-latency")
        print("        artifact, not a sequencer or gas-pricing issue. Consider a different")
        print("        RPC provider or the dedicated sequencer feed Robinhood's own docs")
        print("        mention for full nodes.")
    else:
        print(f"    ==> Inclusion itself genuinely took ~{result['blocks_elapsed']} real blocks —")
        print("        this is NOT a detection/measurement artifact. Given Robinhood Chain's")
        print("        documented FCFS sequencing (gas fee does not affect order), this is")
        print("        unlikely to be a gas-pricing problem specifically — investigate")
        print("        sequencer-side conditions (load, testnet-specific behavior) instead")
        print("        of raising priority_fee_multiplier/max_priority_fee_gwei_cap, which")
        print("        would not be expected to help on this chain's own documented model.")


def main() -> int:
    if len(sys.argv) != 4:
        print(f"usage: {sys.argv[0]} <rpc_url> <tx_hash> <dispatch_to_inclusion_ms>", file=sys.stderr)
        return 1
    rpc_url, tx_hash, dispatch_to_inclusion_ms = sys.argv[1], sys.argv[2], int(sys.argv[3])

    print(f"==> fetching receipt for {tx_hash}")
    receipt = rpc_call(rpc_url, "eth_getTransactionReceipt", [tx_hash])
    if receipt is None:
        print("no receipt found — tx not (yet) included, or wrong RPC/chain", file=sys.stderr)
        return 1

    result = analyze(receipt, lambda n: rpc_call(rpc_url, "eth_getBlockByNumber", [hex(n), False]),
                      dispatch_to_inclusion_ms)
    print_report(result)
    return 0


if __name__ == "__main__":
    sys.exit(main())
