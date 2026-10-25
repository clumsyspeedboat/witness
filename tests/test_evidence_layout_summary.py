"""The promotion decision must not accept incomplete or inconsistent runs."""

import contextlib
import csv
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "experiments/summarize_evidence_layout.py"
SPEC = importlib.util.spec_from_file_location("evidence_summary", MODULE_PATH)
SUMMARY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUMMARY)


class SummaryTests(unittest.TestCase):
    def fixture(self, directory, candidate_time=100, candidate_bytes=70, split="heldout"):
        layouts, queries = [], []
        for layout in SUMMARY.LAYOUTS:
            candidate = layout == "payload_4k"
            size = candidate_bytes if candidate else 100
            layouts.append(dict(pair="0", split=split, group="test", source="source", layout=layout,
                                bytes=size, selector_bytes=size // 2, value_bytes=size - size // 2,
                                pages=1, first_query_ns=100))
            for planner in SUMMARY.PLANNERS:
                time = candidate_time if candidate else 100
                queries.append(dict(pair="0", split=split, group="test", source="source", predicate="1pct",
                                    requested=".01", matched_rows=10, expected_sum=30, layout=layout, planner=planner,
                                    p25_ns=time, median_ns=time, p75_ns=time,
                                    skipped=0, accepted=0, searched=0, translated=0, scanned=1))
        for filename, rows in [("layouts.csv", layouts), ("queries.csv", queries)]:
            self.write_rows(directory / filename, rows)
        (directory / "freeze.csv").write_text("key,value\nstatus,complete\npairs,1\nsources,1\nsmoke,false\n")

    @staticmethod
    def write_rows(path, rows):
        with path.open("w") as stream:
            writer = csv.DictWriter(stream, fieldnames=rows[0].keys())
            writer.writeheader()
            writer.writerows(rows)

    def summarize(self, directory):
        with contextlib.redirect_stdout(io.StringIO()):
            return SUMMARY.summarize(directory)

    def test_only_a_complete_heldout_candidate_passing_both_targets_promotes(self):
        for time, size, split, promotes in [(105, 80, "heldout", True), (106, 80, "heldout", False),
                                            (100, 81, "heldout", False), (100, 70, "smoke", False),
                                            (100, 70, "development", False)]:
            with self.subTest(time=time, size=size, split=split), tempfile.TemporaryDirectory() as temp:
                directory = Path(temp)
                self.fixture(directory, time, size, split)
                self.assertEqual(self.summarize(directory)["promote_to_broader_rerun"], promotes)
                self.assertEqual(json.loads((directory / "decision.json").read_text())["status"], "complete")

    def test_missing_baseline_and_duplicate_cells_are_rejected(self):
        for duplicate in [False, True]:
            with self.subTest(duplicate=duplicate), tempfile.TemporaryDirectory() as temp:
                directory = Path(temp)
                self.fixture(directory)
                with (directory / "queries.csv").open() as stream:
                    rows = list(csv.DictReader(stream))
                rows = rows + [rows[0]] if duplicate else rows[:-1]
                self.write_rows(directory / "queries.csv", rows)
                with self.assertRaises(ValueError):
                    self.summarize(directory)

    def test_smoke_flag_prevents_promotion_even_if_a_split_is_mislabeled(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            self.fixture(directory)
            (directory / "freeze.csv").write_text("key,value\nstatus,complete\npairs,1\nsources,1\nsmoke,true\n")
            self.assertFalse(self.summarize(directory)["promote_to_broader_rerun"])

    def test_mismatching_answers_and_incomplete_actions_are_rejected(self):
        for field, value in [("expected_sum", "31"), ("scanned", "0"), ("median_ns", "nan")]:
            with self.subTest(field=field), tempfile.TemporaryDirectory() as temp:
                directory = Path(temp)
                self.fixture(directory)
                with (directory / "queries.csv").open() as stream:
                    rows = list(csv.DictReader(stream))
                rows[-1][field] = value
                self.write_rows(directory / "queries.csv", rows)
                with self.assertRaises(ValueError):
                    self.summarize(directory)

    def test_unfinished_measurement_is_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            self.fixture(directory)
            (directory / "freeze.csv").write_text("key,value\nstatus,started\n")
            with self.assertRaises(ValueError):
                self.summarize(directory)


if __name__ == "__main__":
    unittest.main()
