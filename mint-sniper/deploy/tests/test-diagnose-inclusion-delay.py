#!/usr/bin/env python3
"""STEP 15 FOLLOW-UP — regression test for
deploy/lib/diagnose_inclusion_delay.py's analyze() function against
fixture blocks, no real RPC/network needed. Scenarios modeled on the
real finding this tool exists to investigate: a confirmed PUSH-path
fire with dispatch_to_inclusion_ms around 2700-3000ms against a
~227ms-measured Robinhood Chain testnet block time (step 14b) — is
that ~12x gap real on-chain delay, or node/subscription lag?
Run manually with:
  python3 deploy/tests/test-diagnose-inclusion-delay.py
"""
import os
import sys

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(SCRIPT_DIR, "..", "lib"))
from diagnose_inclusion_delay import analyze  # noqa: E402

FAILURES = 0


def check(condition: bool, label: str) -> None:
    global FAILURES
    if condition:
        print(f"PASS: {label}")
    else:
        print(f"FAIL: {label}", file=sys.stderr)
        FAILURES += 1


def make_blocks(start_num: int, start_ts: int, count: int, block_time_secs: float) -> dict:
    """A dict of block_number -> fake eth_getBlockByNumber result, evenly
    spaced by block_time_secs (rounded to whole seconds — real timestamps
    are 1-second granular, same as production RPC responses)."""
    blocks = {}
    for i in range(count):
        num = start_num - i
        ts = int(start_ts - i * block_time_secs)
        blocks[num] = {
            "timestamp": hex(ts),
            "baseFeePerGas": hex(1_000_000),  # 0.001 gwei, arbitrary but nonzero
        }
    return blocks


def main() -> int:
    print("=== scenario 1: real on-chain delay (12 blocks at ~227ms each = ~2724ms) ===")
    print("    (this is the actual finding being investigated: a confirmed PUSH-path")
    print("     fire with dispatch_to_inclusion_ms=2722, ~227ms measured block time)")
    inclusion_block_num = 100_000
    inclusion_ts = 1_755_800_100
    blocks = make_blocks(inclusion_block_num, inclusion_ts, count=20, block_time_secs=0.227)
    receipt = {
        "blockNumber": hex(inclusion_block_num),
        "effectiveGasPrice": hex(2_000_000),
        "status": "0x1",
    }
    result = analyze(receipt, lambda n: blocks[n], dispatch_to_inclusion_ms=2722)
    # Loose bound deliberately — block timestamps are 1-second granular in
    # real RPC responses too (same rounding this fixture reproduces), so a
    # sub-second-block-time chain's exact blocks_elapsed count is inherently
    # fuzzy by a block or two either way. What matters for the actual
    # verdict is "clearly more than a couple of blocks," not an exact count.
    check(result["blocks_elapsed"] >= 5, f"12-block-real-delay scenario reports several real blocks elapsed, not <=2 (got {result['blocks_elapsed']})")
    check(not result["likely_node_lag_not_real_delay"], "correctly classified as REAL inclusion delay, not node lag")

    print()
    print("=== scenario 2: fast real inclusion, slow DETECTION (node/subscription lag) ===")
    print("    (the other real possibility: tx landed in the very next block, but this")
    print("     bot's subscribed node took ~2.7s to learn about / push it)")
    # Only 1 real block between dispatch and inclusion (227ms apart), but
    # the caller still passes the full ~2722ms as dispatch_to_inclusion_ms
    # (what the bot actually measured) — analyze() must still correctly
    # find that only 1 block truly separates them, by walking blocks
    # whose OWN timestamps are seconds apart even though the reported
    # duration is much larger.
    blocks_fast = {
        inclusion_block_num: {"timestamp": hex(inclusion_ts), "baseFeePerGas": hex(1_000_000)},
        inclusion_block_num - 1: {"timestamp": hex(inclusion_ts - 1), "baseFeePerGas": hex(1_000_000)},
        inclusion_block_num - 2: {"timestamp": hex(inclusion_ts - 4), "baseFeePerGas": hex(1_000_000)},
    }
    result = analyze(receipt, lambda n: blocks_fast[n], dispatch_to_inclusion_ms=2722)
    check(result["likely_node_lag_not_real_delay"], "correctly classified as node/detection lag, not real inclusion delay")

    print()
    print("=== scenario 3: gas price note — effectiveGasPrice at or below baseFeePerGas ===")
    receipt_underpriced = {
        "blockNumber": hex(inclusion_block_num),
        "effectiveGasPrice": hex(1_000_000),  # equals baseFeePerGas exactly
        "status": "0x1",
    }
    result = analyze(receipt_underpriced, lambda n: blocks[n], dispatch_to_inclusion_ms=500)
    check(result["priority_paid"] == 0, "priority_paid computed correctly as 0 when effectiveGasPrice == baseFeePerGas")

    print()
    print("=== scenario 4: no baseFeePerGas in the block response (legacy/non-1559) ===")
    blocks_no_basefee = {inclusion_block_num: {"timestamp": hex(inclusion_ts)}}
    # dispatch_to_inclusion_ms=0 keeps the block-walk loop from stepping
    # past the single fixture block provided here — this scenario is
    # about the missing-baseFeePerGas handling, not the walk logic
    # (already covered by scenarios 1-2).
    result = analyze(receipt, lambda n: blocks_no_basefee[n], dispatch_to_inclusion_ms=0)
    check(result["base_fee_at_inclusion"] is None, "handles a missing baseFeePerGas without crashing")
    check(result["priority_paid"] is None, "priority_paid is None when there's no base fee to compare against")

    print()
    print("=== scenario 5: reverted tx still analyzed correctly ===")
    receipt_reverted = {
        "blockNumber": hex(inclusion_block_num),
        "effectiveGasPrice": hex(2_000_000),
        "status": "0x0",
    }
    result = analyze(receipt_reverted, lambda n: blocks[n], dispatch_to_inclusion_ms=500)
    check(result["status_ok"] is False, "a reverted tx (status=0x0) is reported as not ok, not treated as a crash")

    if FAILURES == 0:
        print("\nALL TESTS PASSED")
        return 0
    print(f"\n{FAILURES} TEST(S) FAILED", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
