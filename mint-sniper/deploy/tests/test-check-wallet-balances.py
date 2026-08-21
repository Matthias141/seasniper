#!/usr/bin/env python3
"""STEP 15e FOLLOW-UP — regression test for
deploy/lib/check_wallet_balances.py against mocked GET /api/status
JSON, no real bot or network needed. This is the pre-flight balance
gate that was specified for step 15e but never actually built — the
live incident it responds to was three real wallets correctly
reporting 0.000000000000000000 balance without the script stopping.
Run manually with:
  python3 deploy/tests/test-check-wallet-balances.py
"""
import json
import os
import subprocess
import sys

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
CHECK_SCRIPT = os.path.join(SCRIPT_DIR, "..", "lib", "check_wallet_balances.py")

FAILURES = 0


def check(condition: bool, label: str) -> None:
    global FAILURES
    if condition:
        print(f"PASS: {label}")
    else:
        print(f"FAIL: {label}", file=sys.stderr)
        FAILURES += 1


def run(status: dict, threshold: str = "0.01") -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, CHECK_SCRIPT, threshold],
        input=json.dumps(status),
        capture_output=True,
        text=True,
    )


def main() -> int:
    print("=== the exact live incident: three wallets at 0 balance ===")
    zero_balance_status = {
        "armed": False,
        "wallets": [
            {"address": "0xAAA", "balance_eth": "0.000000000000000000", "nonce": 0, "healthy": True},
            {"address": "0xBBB", "balance_eth": "0.000000000000000000", "nonce": 0, "healthy": True},
            {"address": "0xCCC", "balance_eth": "0.000000000000000000", "nonce": 0, "healthy": True},
        ],
    }
    result = run(zero_balance_status)
    check(result.returncode == 0, "script itself exits 0 (caller decides what the output means)")
    check("0xAAA" in result.stdout, "flags the first underfunded wallet")
    check("0xBBB" in result.stdout, "flags the second underfunded wallet")
    check("0xCCC" in result.stdout, "flags the third underfunded wallet")
    check(result.stdout.count("\n") == 3, "exactly 3 lines of output for 3 underfunded wallets")

    print("\n=== all wallets sufficiently funded ===")
    funded_status = {
        "armed": False,
        "wallets": [
            {"address": "0xAAA", "balance_eth": "0.500000000000000000", "nonce": 0, "healthy": True},
        ],
    }
    result = run(funded_status)
    check(result.returncode == 0, "script exits 0")
    check(result.stdout == "", "no output at all when every wallet clears the threshold")

    print("\n=== boundary: exactly at the threshold is NOT underfunded (strict less-than) ===")
    boundary_status = {
        "armed": False,
        "wallets": [{"address": "0xAAA", "balance_eth": "0.010000000000000000", "nonce": 0, "healthy": True}],
    }
    result = run(boundary_status, threshold="0.01")
    check(result.stdout == "", "a balance exactly equal to the threshold passes")

    print("\n=== just below the threshold IS underfunded ===")
    just_below_status = {
        "armed": False,
        "wallets": [{"address": "0xAAA", "balance_eth": "0.009999999999999999", "nonce": 0, "healthy": True}],
    }
    result = run(just_below_status, threshold="0.01")
    check("0xAAA" in result.stdout, "a balance just below the threshold is flagged")

    print("\n=== mixed: only the underfunded wallet is reported, not the funded one ===")
    mixed_status = {
        "armed": False,
        "wallets": [
            {"address": "0xFUNDED", "balance_eth": "1.0", "nonce": 0, "healthy": True},
            {"address": "0xDRY", "balance_eth": "0.0", "nonce": 0, "healthy": True},
        ],
    }
    result = run(mixed_status)
    check("0xDRY" in result.stdout, "the dry wallet is reported")
    check("0xFUNDED" not in result.stdout, "the funded wallet is NOT reported")

    print("\n=== no wallets configured at all ===")
    empty_status = {"armed": False, "wallets": []}
    result = run(empty_status)
    check(result.returncode == 0, "script handles an empty wallet list without crashing")
    check(result.stdout == "", "no output for an empty wallet list")

    if FAILURES == 0:
        print("\nALL TESTS PASSED")
        return 0
    print(f"\n{FAILURES} TEST(S) FAILED", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
