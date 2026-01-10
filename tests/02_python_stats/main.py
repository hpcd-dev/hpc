import csv
import json
import pathlib
import time

DATA_PATH = pathlib.Path("data/input.csv")
RESULTS_DIR = pathlib.Path("results")

values = []
with DATA_PATH.open(newline="") as handle:
    reader = csv.DictReader(handle)
    for row in reader:
        values.append(float(row["value"]))

RESULTS_DIR.mkdir(parents=True, exist_ok=True)
count = len(values)
summary = {
    "count": count,
    "min": min(values),
    "max": max(values),
    "mean": sum(values) / count,
}

with (RESULTS_DIR / "stats.json").open("w") as handle:
    json.dump(summary, handle, indent=2, sort_keys=True)
    handle.write("\n")

with (RESULTS_DIR / "preview.txt").open("w") as handle:
    handle.write("values:\n")
    for value in values[:5]:
        handle.write(f"- {value}\n")

# Small delay for testing job status while running.
time.sleep(2)
