"""Export Criterion estimates; generated CSV is evidence, not a live-heap claim.

Usage: rtk proxy python3 crates/core/benches/support/summarize.py before docs/transcript-performance-baseline.csv
"""
import csv
import json
import pathlib
import sys

baseline, destination = sys.argv[1:]
rows = []
for path in pathlib.Path("target/criterion").glob(f"**/{baseline}/estimates.json"):
    data = json.loads(path.read_text())
    metadata = json.loads(path.with_name("benchmark.json").read_text())
    samples = json.loads(path.with_name("sample.json").read_text())
    estimate = data.get("slope") or data["mean"]
    confidence = estimate["confidence_interval"]
    rows.append([
        metadata["full_id"], len(samples["iters"]), sum(samples["iters"]),
        confidence["lower_bound"], estimate["point_estimate"], confidence["upper_bound"],
        data["median"]["point_estimate"], data["std_dev"]["point_estimate"],
    ])
with pathlib.Path(destination).open("w", newline="") as output:
    writer = csv.writer(output, lineterminator="\n")
    writer.writerow(["benchmark", "samples", "iterations", "low_ns", "estimate_ns", "high_ns", "median_ns", "stddev_ns"])
    writer.writerows(sorted(rows))
print(f"Exported {len(rows)} measurements to {destination}")
