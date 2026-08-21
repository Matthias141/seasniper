#!/usr/bin/env python3
"""STEP 15e FOLLOW-UP — the pre-flight balance gate that was specified
for step 15e but never actually built: reads a bot GET /api/status JSON
response from stdin and prints one line per wallet below the given
threshold. Pulled out of run-benchmark.sh into its own file so it's
testable on its own (deploy/tests/test-check-wallet-balances.py)
against mocked /api/status JSON, without needing a real bot or network
at all.

Usage: check_wallet_balances.py <min_balance_eth>
Reads: a GET /api/status JSON body on stdin (state::AppState's shape —
       {"armed": bool, "wallets": [{"address":..., "balance_eth":...}, ...]})
Prints: one line per underfunded wallet: "<address> balance=<bal> (need >= <threshold>)"
Exit code: 0 always (the caller decides what an empty vs. non-empty
           result means — this script only reports, never gates on its
           own, matching run-benchmark.sh's own "print then check
           output" pattern for everything else it shells out to).
"""
import json
import sys


def find_underfunded(status: dict, threshold: float) -> list[str]:
    lines = []
    for wallet in status.get("wallets", []):
        balance = float(wallet["balance_eth"])
        if balance < threshold:
            lines.append(f"{wallet['address']} balance={wallet['balance_eth']} (need >= {threshold})")
    return lines


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <min_balance_eth>", file=sys.stderr)
        return 1
    threshold = float(sys.argv[1])

    status = json.load(sys.stdin)
    for line in find_underfunded(status, threshold):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
