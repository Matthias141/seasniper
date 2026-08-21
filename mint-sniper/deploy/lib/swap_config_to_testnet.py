#!/usr/bin/env python3
"""STEP 15e FOLLOW-UP — rewrites config.toml's network/target fields to
point at Robinhood Chain testnet + the confirmed-live benchmark
contract, in place. Pulled out of run-benchmark.sh into its own file
specifically so it's testable on its own (deploy/tests/
test-swap-config-to-testnet.py) without needing systemd, sudo, or a
real bot process — this is pure text transformation, no privileged
operations here at all (run-benchmark.sh handles the chown/restart
around this call).

Usage: swap_config_to_testnet.py <config_path> <ws_url> <http_url> <nft_contract>
"""
import re
import sys


def swap(content: str, ws_url: str, http_url: str, nft_contract: str) -> str:
    def set_line(content: str, key: str, value_literal: str) -> str:
        pattern = re.compile(rf'^{re.escape(key)}\s*=.*$', re.MULTILINE)
        replacement = f'{key} = {value_literal}'
        new_content, count = pattern.subn(replacement, content, count=1)
        if count == 0:
            raise ValueError(f"could not find a '{key} = ...' line to replace")
        return new_content

    content = set_line(content, "ws_rpc_url", f'"{ws_url}"')

    # http_rpc_urls is a TOML array, possibly spanning multiple lines in
    # the real file (config.example.toml's own default shape) — matched
    # non-greedily from the opening bracket to the first closing one,
    # which is safe here since these are flat string arrays with no
    # nested brackets.
    pattern = re.compile(r'^http_rpc_urls\s*=\s*\[.*?\]', re.MULTILINE | re.DOTALL)
    content, count = pattern.subn(f'http_rpc_urls = ["{http_url}"]', content, count=1)
    if count == 0:
        raise ValueError("could not find an 'http_rpc_urls = [...]' array to replace")

    content = set_line(content, "mint_mode", '"seadrop"')
    content = set_line(content, "nft_contract", f'"{nft_contract}"')
    content = set_line(content, "fee_recipient", '"0x0000a26b00c1F0DF003000390027140000fAa719"')
    content = set_line(content, "quantity_per_wallet", "1")
    content = set_line(content, "block_time_ms", "227")  # step 14b's measured Robinhood testnet figure

    return content


def main() -> int:
    if len(sys.argv) != 5:
        print(f"usage: {sys.argv[0]} <config_path> <ws_url> <http_url> <nft_contract>", file=sys.stderr)
        return 1
    path, ws_url, http_url, nft_contract = sys.argv[1:5]

    with open(path) as f:
        content = f.read()

    try:
        content = swap(content, ws_url, http_url, nft_contract)
    except ValueError as e:
        print(f"error: {e} in {path}", file=sys.stderr)
        return 1

    with open(path, "w") as f:
        f.write(content)

    print("config.toml swapped to testnet in place")
    return 0


if __name__ == "__main__":
    sys.exit(main())
