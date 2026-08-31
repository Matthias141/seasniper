#!/usr/bin/env python3
"""STEP 32b — the percentile/mean summary math that used to live as an
inline heredoc in run-benchmark.sh, pulled out so it's testable on its
own (deploy/tests/test-summarize-results.py) against synthetic data,
same "independently testable, no drift between what's tested and what
actually runs" reasoning as check_wallet_balances.py/find_mint_result.sh.

STEP 32b also re-verified (and this file's own tests assert) that
percentile() has no assumption baked in for any specific sample size —
it was originally only ever exercised at n=15; step 32 is preparing for
a real n=100 run.

STEP 32c adds p99 and a plain arithmetic mean to the printed summary,
alongside the existing p50/p90 (additive, nothing removed) — a 100-
sample run finally has enough data for p99 to mean something.

Usage: summarize_results.py <results_file>
Reads: a file of newline-delimited JSON records, one per successful
       find_mint_result.sh match during a run-benchmark.sh run — each
       record is a flattened audit.log mint_result entry (AuditRecord's
       #[serde(flatten)] means success/send_to_ack_ms/
       dispatch_to_inclusion_ms are top-level keys, not nested under a
       "detail" object — see find_mint_result.sh's own header for the
       live bug that established this).
Prints: the same "successes: k/n" + per-metric p50/p90/p99/mean lines
        run-benchmark.sh has always printed, now via this file.
Exit code: 0 always — this only formats and prints; run-benchmark.sh's
           own bash-side line (using $N, the attempted count — not
           available to this script, which only ever sees what actually
           got recorded) is what reports attempted-vs-recorded.
"""
import json
import sys


def percentile(values: list[float], p: float) -> float | None:
    """Linear-interpolation percentile (same method as the 'linear'
    default most stats libraries use) — correct for any sample size,
    not just 15. A single value returns itself for any p; otherwise
    interpolates between the two nearest ranks. `values` need not be
    pre-sorted."""
    if not values:
        return None
    s = sorted(values)
    if len(s) == 1:
        return s[0]
    k = (len(s) - 1) * (p / 100)
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def mean(values: list[float]) -> float | None:
    if not values:
        return None
    return sum(values) / len(values)


def summarize(records: list[dict]) -> dict:
    send_ack: list[float] = []
    dispatch_incl: list[float] = []
    successes = 0
    total = 0
    for rec in records:
        total += 1
        if rec.get("success"):
            successes += 1
        sa = rec.get("send_to_ack_ms")
        di = rec.get("dispatch_to_inclusion_ms")
        if sa is not None:
            send_ack.append(sa)
        if di is not None:
            dispatch_incl.append(di)
    return {
        "successes": successes,
        "total": total,
        "send_to_ack_ms": send_ack,
        "dispatch_to_inclusion_ms": dispatch_incl,
    }


def format_metric_line(label: str, values: list[float]) -> str:
    if not values:
        return f"{label}: no data"
    p50 = percentile(values, 50)
    p90 = percentile(values, 90)
    p99 = percentile(values, 99)
    m = mean(values)
    return f"{label}: p50={p50:.0f}ms p90={p90:.0f}ms p99={p99:.0f}ms mean={m:.0f}ms (n={len(values)})"


def read_records(path: str) -> list[dict]:
    records = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            records.append(json.loads(line))
    return records


def main() -> int:
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <results_file>", file=sys.stderr)
        return 1

    records = read_records(sys.argv[1])
    result = summarize(records)

    print(f"successes: {result['successes']}/{result['total']}")
    print(format_metric_line("send_to_ack_ms", result["send_to_ack_ms"]))
    print(format_metric_line("dispatch_to_inclusion_ms", result["dispatch_to_inclusion_ms"]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
