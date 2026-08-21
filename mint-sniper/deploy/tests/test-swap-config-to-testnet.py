#!/usr/bin/env python3
"""STEP 15e FOLLOW-UP — regression test for
deploy/lib/swap_config_to_testnet.py against a scratch config file, no
systemd/sudo/network needed at all. Run manually with:
  python3 deploy/tests/test-swap-config-to-testnet.py
"""
import os
import subprocess
import sys
import tempfile

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
SWAP_SCRIPT = os.path.join(SCRIPT_DIR, "..", "lib", "swap_config_to_testnet.py")

# A minimal but realistic scratch config — same field shapes as the
# real config.example.toml, including a multi-line http_rpc_urls array
# (the actual shape found in that file) and a commented-out example
# block further down containing the SAME key names this script must
# NOT touch (this is exactly what tripped up naive regexes elsewhere in
# this project's history — verified explicitly here, not assumed safe).
SAMPLE_CONFIG = '''# Copy to config.toml. NEVER commit config.toml — it holds private keys.

ws_rpc_url = "wss://eth-mainnet.g.alchemy.com/v2/REAL_MAINNET_KEY"
http_rpc_urls = [
  "https://eth-mainnet.g.alchemy.com/v2/REAL_MAINNET_KEY",
  "https://rpc.flashbots.net/fast",
]

mint_mode = "custom"
contract_address = "0x000000000000000000000000000000000000dEaD"
mint_fn_signature = "mint(uint256)"
mint_fn_args_template = ["1"]

seadrop_address = ""
nft_contract = ""
fee_recipient = ""
quantity_per_wallet = 1

block_time_ms = 12000

[[wallets]]
private_key_env = "SNIPER_PK_1"

# --- Example: targeting Robinhood Chain instead of Ethereum mainnet ---
# ws_rpc_url = "wss://robinhood-testnet.g.alchemy.com/v2/YOUR_KEY"
# http_rpc_urls = ["https://robinhood-testnet.g.alchemy.com/v2/YOUR_KEY"]
# nft_contract = "0xSomeOtherAddressThatShouldNeverBeTouched"
'''

FAILURES = 0


def check(condition: bool, label: str) -> None:
    global FAILURES
    if condition:
        print(f"PASS: {label}")
    else:
        print(f"FAIL: {label}", file=sys.stderr)
        FAILURES += 1


def main() -> int:
    with tempfile.NamedTemporaryFile(mode="w", suffix=".toml", delete=False) as f:
        f.write(SAMPLE_CONFIG)
        config_path = f.name

    try:
        result = subprocess.run(
            [
                sys.executable,
                SWAP_SCRIPT,
                config_path,
                "wss://robinhood-testnet.g.alchemy.com/v2/TESTNET_KEY",
                "https://robinhood-testnet.g.alchemy.com/v2/TESTNET_KEY",
                "0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9",
            ],
            capture_output=True,
            text=True,
        )
        check(result.returncode == 0, f"swap script exits 0 (stderr: {result.stderr.strip()})")

        with open(config_path) as f:
            swapped = f.read()

        check(
            'ws_rpc_url = "wss://robinhood-testnet.g.alchemy.com/v2/TESTNET_KEY"' in swapped,
            "ws_rpc_url swapped to the testnet URL",
        )
        check(
            'http_rpc_urls = ["https://robinhood-testnet.g.alchemy.com/v2/TESTNET_KEY"]' in swapped,
            "multi-line http_rpc_urls array collapsed to the single testnet URL",
        )
        check('mint_mode = "seadrop"' in swapped, "mint_mode swapped to seadrop")
        check(
            'nft_contract = "0xf926f5B2e0b760807f032e0C4fC8876c2FF245C9"' in swapped,
            "nft_contract swapped to the benchmark address",
        )
        check(
            'fee_recipient = "0x0000a26b00c1F0DF003000390027140000fAa719"' in swapped,
            "fee_recipient swapped to OpenSea's default",
        )
        check("quantity_per_wallet = 1" in swapped, "quantity_per_wallet set to 1")
        check("block_time_ms = 227" in swapped, "block_time_ms swapped to the measured testnet figure")

        # The real bug class this guards against: a naive regex touching
        # a COMMENTED-OUT example line that happens to share a key name.
        check(
            "0xSomeOtherAddressThatShouldNeverBeTouched" in swapped,
            "the commented-out example nft_contract line was left untouched",
        )
        check(
            "REAL_MAINNET_KEY" not in swapped,
            "the original mainnet key is gone from both ws_rpc_url and http_rpc_urls",
        )
        check(
            'contract_address = "0x000000000000000000000000000000000000dEaD"' in swapped,
            "unrelated fields (contract_address) are untouched",
        )
        check(
            'private_key_env = "SNIPER_PK_1"' in swapped,
            "the [[wallets]] table is untouched",
        )
    finally:
        os.unlink(config_path)

    # Second run: missing key should fail loudly, not silently no-op.
    with tempfile.NamedTemporaryFile(mode="w", suffix=".toml", delete=False) as f:
        f.write("# no ws_rpc_url in this file at all\nmint_mode = \"custom\"\n")
        broken_path = f.name
    try:
        result = subprocess.run(
            [sys.executable, SWAP_SCRIPT, broken_path, "wss://x", "https://x", "0xdead"],
            capture_output=True,
            text=True,
        )
        check(result.returncode != 0, "swap script fails loudly when ws_rpc_url is missing entirely")
    finally:
        os.unlink(broken_path)

    if FAILURES == 0:
        print("\nALL TESTS PASSED")
        return 0
    print(f"\n{FAILURES} TEST(S) FAILED", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
