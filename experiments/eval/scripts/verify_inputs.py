#!/usr/bin/env python3
"""Verify both the contents and inventory of the claim-bearing study inputs."""

import argparse
import hashlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
DATA = ROOT / "experiments/eval/data"
MANIFEST = ROOT / "INPUTS.sha256"

# Match the directories enumerated by src/experiment/datasets.rs. Extra CSVs
# matter too: readers discover them dynamically and would change the corpus.
PATTERNS = (
    "nab/realAWSCloudwatch/*.csv",
    "nab/realAdExchange/*.csv",
    "nab/realKnownCause/*.csv",
    "nab/realTraffic/*.csv",
    "nab/realTweets/*.csv",
    "tpch/*.csv",
    "clickbench/*.csv",
    "publicbi/*.csv",
    "census/clickbench/*.csv",
    "census/publicbi/*.csv",
    "census/tpcds/*.csv",
    "census/tpch/*.csv",
    "scale/tpch_sf10/*.csv",
    "strings/clickbench/*.parquet",
    "strings/publicbi/*.parquet",
    "strings/tpcds/*.parquet",
    "strings/tpch/*.parquet",
)
FIXED = (
    "realKnownCause__nyc_taxi.csv",
    "realKnownCause__ambient_temperature_system_failure.csv",
    "realTweets__Twitter_volume_AAPL.csv",
    "household_power_consumption.txt",
    "yellow_tripdata_2024-01.parquet",
)


def inventory(data):
    paths = {name for name in FIXED if (data / name).is_file()}
    paths.update(
        path.relative_to(data).as_posix()
        for pattern in PATTERNS
        for path in data.glob(pattern)
        if path.is_file()
    )
    return paths


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def verify(data=DATA, manifest=MANIFEST):
    expected = {}
    for line in manifest.read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        checksum, name = line.split("  ", 1)
        if name in expected:
            raise ValueError(f"duplicate manifest input: {name}")
        expected[name] = checksum
    actual = inventory(data)
    problems = [f"missing input: {name}" for name in sorted(expected.keys() - actual)]
    problems += [f"unlisted input: {name}" for name in sorted(actual - expected.keys())]
    if problems:
        raise ValueError("\n".join(problems))
    for name, checksum in expected.items():
        if digest(data / name) != checksum:
            raise ValueError(f"input checksum mismatch: {name}")
    return len(expected)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", type=Path, default=DATA)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    args = parser.parse_args()
    try:
        count = verify(args.data, args.manifest)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\nCorpus validation failed; refusing to measure.\n")
    print(f"Verified {count} study inputs and the complete input inventory.")


if __name__ == "__main__":
    main()
