#!/usr/bin/env python3
"""STEP 15e FOLLOW-UP — rewrites config.toml's network/target fields to
point at Robinhood Chain testnet + the confirmed-live benchmark
contract, in place. Pulled out of run-benchmark.sh into its own file
specifically so it's testable on its own (deploy/tests/
test-swap-config-to-testnet.py) without needing systemd, sudo, or a
real bot process — this is pure text transformation, no privileged
operations here at all (run-benchmark.sh handles the chown/restart
around this call).

STEP 28 (final) — race_mode/sequencer_http_url are now part of the
swap, not left to whatever the pre-swap config happened to have. Two
earlier steps in this project's history explicitly chose NOT to do
this ("forcing race_mode to a specific value here would change this
script's behavior beyond what was asked and isn't needed — reporting
what's actually configured is the real gap") — that reasoning held when
the goal was a general-purpose speed benchmark. It no longer holds now
that the specific, stated goal of the next run is confirming whether
race_mode/sequencer racing caused a previously-observed 5.5x
dispatch_to_inclusion improvement: leaving it to chance is exactly the
mechanism that made the LAST attempt at this unconfirmable (see
CLAUDE.md's step 28 section — the operator's baseline config.toml
before the swap is not something this script has ever had visibility
into, and nothing forced it to actually match the feature under test).
Defaults to enabled (`race_mode=True` in `swap()`, `"true"` if the
optional 5th CLI arg is omitted) — override with `race_mode=false` (CLI:
a literal `false` 5th argument) for a future benchmark that
deliberately wants the non-race-mode path instead.

`sequencer_http_url` is forced to Robinhood Chain testnet's real,
confirmed sequencer endpoint (`docs.robinhood.com`-documented, already
referenced in `config.example.toml`'s own Robinhood Chain section) —
never left as whatever arbitrary value a config might have had. When
race_mode is enabled, `jitter_ms_min`/`jitter_ms_max`/`gas_jitter_pct`
are also forced to 0 — required by `Config::validate()`'s own
race_mode invariant (`config.rs`: race_mode rejects any nonzero jitter
field), so a race_mode-forced swap that left jitter untouched would
produce a config.toml the bot refuses to boot on, defeating the whole
point. `race_mode`/`sequencer_http_url` use set-OR-APPEND semantics
(unlike every other field this script touches): both are optional
`#[serde(default)]` `Config` fields (config.rs), so an older config.toml
predating this feature may genuinely lack either line entirely — the
existing strict "must already exist" `set_line` used for every other
field would wrongly hard-fail on that shape. `jitter_ms_min`/
`jitter_ms_max`/`gas_jitter_pct` have no such default in `Config` — any
config.toml that loads at all already has all three — so those keep
using the strict, must-already-exist form.

Usage: swap_config_to_testnet.py <config_path> <ws_url> <http_url> <nft_contract> [race_mode: true|false]
"""
import re
import sys

# docs.robinhood.com/chain — the real, confirmed Robinhood Chain testnet
# sequencer endpoint (also referenced in config.example.toml's own
# Robinhood Chain section). Never a value this script invents.
SEQUENCER_TESTNET_URL = "https://sequencer.testnet.chain.robinhood.com"


def set_line(content: str, key: str, value_literal: str) -> str:
    """Strict: the `key = ...` line MUST already exist, or this raises.
    Correct for every `Config` field that has no `#[serde(default)]` —
    a config.toml that loads at all already has these."""
    pattern = re.compile(rf'^{re.escape(key)}\s*=.*$', re.MULTILINE)
    replacement = f'{key} = {value_literal}'
    new_content, count = pattern.subn(replacement, content, count=1)
    if count == 0:
        raise ValueError(f"could not find a '{key} = ...' line to replace")
    return new_content


def set_or_append_line(content: str, key: str, value_literal: str) -> str:
    """For `Config` fields with `#[serde(default)]` (currently just
    race_mode/sequencer_http_url) — an older config.toml predating a
    feature may genuinely never have had this line at all, which is a
    real, valid, loadable shape (the default applies), not an error.
    Appends a fresh line at the end of the file when the key is
    missing, rather than raising like set_line does."""
    pattern = re.compile(rf'^{re.escape(key)}\s*=.*$', re.MULTILINE)
    replacement = f'{key} = {value_literal}'
    new_content, count = pattern.subn(replacement, content, count=1)
    if count == 0:
        separator = '' if content.endswith('\n') else '\n'
        return f'{content}{separator}{replacement}\n'
    return new_content


def swap(content: str, ws_url: str, http_url: str, nft_contract: str, race_mode: bool = True) -> str:
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

    if race_mode:
        content = set_or_append_line(content, "race_mode", "true")
        content = set_or_append_line(content, "sequencer_http_url", f'"{SEQUENCER_TESTNET_URL}"')
        # Config::validate()'s own race_mode invariant — required, not
        # optional, or the swapped config.toml fails to boot at all.
        content = set_line(content, "jitter_ms_min", "0")
        content = set_line(content, "jitter_ms_max", "0")
        content = set_line(content, "gas_jitter_pct", "0")
    else:
        content = set_or_append_line(content, "race_mode", "false")

    return content


def main() -> int:
    if len(sys.argv) not in (5, 6):
        print(
            f"usage: {sys.argv[0]} <config_path> <ws_url> <http_url> <nft_contract> [race_mode: true|false]",
            file=sys.stderr,
        )
        return 1
    path, ws_url, http_url, nft_contract = sys.argv[1:5]
    race_mode = True
    if len(sys.argv) == 6:
        race_mode = sys.argv[5].strip().lower() == "true"

    with open(path) as f:
        content = f.read()

    try:
        content = swap(content, ws_url, http_url, nft_contract, race_mode)
    except ValueError as e:
        print(f"error: {e} in {path}", file=sys.stderr)
        return 1

    with open(path, "w") as f:
        f.write(content)

    print(f"config.toml swapped to testnet in place (race_mode={'true' if race_mode else 'false'})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
