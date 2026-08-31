#!/usr/bin/env python3
"""STEP 32b — regression test for deploy/lib/summarize_results.py's
percentile()/mean()/summarize() functions, directly imported (same
pattern as test-diagnose-inclusion-delay.py), plus a CLI-level
subprocess check of the full printed summary (p50/p90/p99/mean, step
32c). This is what re-verifies the percentile math actually holds at a
meaningfully larger sample size (n=100), not just the n=15 this
function was originally only ever exercised at (step 32's own
motivation) — no real bot, RPC, or audit.log needed.
Run manually with:
  python3 deploy/tests/test-summarize-results.py
"""
import json
import os
import subprocess
import sys
import tempfile

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(SCRIPT_DIR, "..", "lib"))
from summarize_results import format_metric_line, mean, percentile, summarize  # noqa: E402

SUMMARIZE_SCRIPT = os.path.join(SCRIPT_DIR, "..", "lib", "summarize_results.py")

FAILURES = 0


def check(condition: bool, label: str) -> None:
    global FAILURES
    if condition:
        print(f"PASS: {label}")
    else:
        print(f"FAIL: {label}", file=sys.stderr)
        FAILURES += 1


def main() -> int:
    print("=== percentile(): known-good small cases, cross-checked by hand ===")
    check(percentile([], 50) is None, "empty list returns None")
    check(percentile([42], 50) == 42, "single value returns itself for p50")
    check(percentile([42], 99) == 42, "single value returns itself for p99 too")
    # [10, 20, 30, 40, 50] — p50 (median) of 5 values is the middle one.
    five = [10, 20, 30, 40, 50]
    check(percentile(five, 50) == 30, "p50 of 5 evenly-spaced values is the middle value")
    check(percentile(five, 0) == 10, "p0 is the minimum")
    check(percentile(five, 100) == 50, "p100 is the maximum")

    print("\n=== percentile(): n=100 synthetic values, no assumption baked in for n=15 ===")
    # 1..100 — p50 should land at/near 50.5 (linear interpolation between
    # the 50th and 51st values, standard "linear" method), p90 near 90.1,
    # p99 near 99.01. Cross-checked independently: k = (100-1)*(p/100).
    hundred = list(range(1, 101))  # 1, 2, ..., 100
    p50 = percentile(hundred, 50)
    p90 = percentile(hundred, 90)
    p99 = percentile(hundred, 99)
    check(p50 is not None and abs(p50 - 50.5) < 0.01, f"p50 of 1..100 is ~50.5 (got {p50})")
    check(p90 is not None and abs(p90 - 90.1) < 0.01, f"p90 of 1..100 is ~90.1 (got {p90})")
    check(p99 is not None and abs(p99 - 99.01) < 0.01, f"p99 of 1..100 is ~99.01 (got {p99})")
    check(percentile(hundred, 0) == 1, "p0 of 1..100 is 1")
    check(percentile(hundred, 100) == 100, "p100 of 1..100 is 100")

    print("\n=== percentile(): order independence — unsorted input matches sorted input ===")
    import random

    shuffled = hundred[:]
    random.Random(42).shuffle(shuffled)
    check(percentile(shuffled, 50) == percentile(hundred, 50), "shuffled n=100 gives the same p50 as sorted")
    check(percentile(shuffled, 90) == percentile(hundred, 90), "shuffled n=100 gives the same p90 as sorted")

    print("\n=== mean() ===")
    check(mean([]) is None, "empty list returns None")
    check(mean([10]) == 10, "single value is its own mean")
    check(mean(hundred) == 50.5, "mean of 1..100 is 50.5")

    print("\n=== summarize(): a realistic n=100 mix of success/failure records ===")
    records = []
    for i in range(100):
        success = i % 10 != 0  # 90 successes, 10 failures
        rec = {"success": success, "send_to_ack_ms": 150 + i, "dispatch_to_inclusion_ms": 300 + i * 2}
        records.append(rec)
    result = summarize(records)
    check(result["successes"] == 90, "90/100 successes counted correctly")
    check(result["total"] == 100, "total is 100, not hardcoded to 15")
    check(len(result["send_to_ack_ms"]) == 100, "all 100 send_to_ack_ms values collected")
    check(len(result["dispatch_to_inclusion_ms"]) == 100, "all 100 dispatch_to_inclusion_ms values collected")

    print("\n=== summarize(): a TimedOut record (dispatch_to_inclusion_ms: null) is excluded, not zeroed ===")
    mixed = [
        {"success": True, "send_to_ack_ms": 100, "dispatch_to_inclusion_ms": 200},
        {"success": False, "send_to_ack_ms": 110, "dispatch_to_inclusion_ms": None},
    ]
    result = summarize(mixed)
    check(result["total"] == 2, "both records counted toward total")
    check(len(result["send_to_ack_ms"]) == 2, "both send_to_ack_ms values present")
    check(len(result["dispatch_to_inclusion_ms"]) == 1, "the null dispatch_to_inclusion_ms is excluded, not treated as 0")

    print("\n=== format_metric_line(): p99 and mean are present (step 32c), alongside p50/p90 ===")
    line = format_metric_line("send_to_ack_ms", [100, 150, 200, 250, 300])
    check("p50=" in line, "p50 present")
    check("p90=" in line, "p90 present")
    check("p99=" in line, "p99 present (step 32c)")
    check("mean=" in line, "mean present (step 32c)")
    check(format_metric_line("x", []) == "x: no data", "no-data case unchanged")

    print("\n=== CLI-level: a real n=100 results file through the actual script ===")
    with tempfile.NamedTemporaryFile(mode="w", suffix=".jsonl", delete=False) as tf:
        for i in range(100):
            success = i % 20 != 0  # 95 successes, 5 failures
            rec = {
                "ts": 1755800000 + i,
                "event": "mint_result",
                "address": f"0x{i:040x}",
                "success": success,
                "detail": "confirmed" if success else "timed out",
                "send_to_ack_ms": 130 + (i % 50),
                "dispatch_to_inclusion_ms": 900 + (i % 400),
            }
            tf.write(json.dumps(rec) + "\n")
        results_path = tf.name
    try:
        proc = subprocess.run([sys.executable, SUMMARIZE_SCRIPT, results_path], capture_output=True, text=True)
        check(proc.returncode == 0, "script exits 0 on a real n=100 file")
        check("successes: 95/100" in proc.stdout, "successes line reports 95/100, not truncated at 15")
        check("send_to_ack_ms: p50=" in proc.stdout and "p99=" in proc.stdout, "send_to_ack_ms line has p50..p99")
        check("dispatch_to_inclusion_ms: p50=" in proc.stdout and "mean=" in proc.stdout, "dispatch_to_inclusion_ms line has p50..mean")
        check("(n=100)" in proc.stdout, "both metric lines report n=100, matching the real sample size")
    finally:
        os.unlink(results_path)

    if FAILURES == 0:
        print("\nALL TESTS PASSED")
        return 0
    print(f"\n{FAILURES} TEST(S) FAILED", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
