import copy
import csv
import importlib.util
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("query_optimization_summary", ROOT / "experiments/summarize_query_optimization.py")
SUMMARY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SUMMARY)
FINGERPRINT = "0123456789abcdef"
PROTOCOL = "prepared_selective_synthetic_at_least10pct_gain_heldout_direct_sourceweighted_ratio_at_most1.05_cost_heldout_geo_at_most0.95_sourceweighted_at_most1_regressions20pct_at_most5pct"


def write_rows(path, rows):
    with path.open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def fixture(directory):
    freeze = {
        "status": "complete", "rule_fingerprint": FINGERPRINT, "mode": "pilot", "rounds": "9",
        "arm_count": "11", "row_cap": "1048576", "cost_gate": "no_measured_query_tuning",
        "warm_scope": "resident_after_initialization_regions_and_preparation",
        "total_scope": "resident_bytes_to_answer_including_both_columns_optional_regions_preparation_and_teardown",
        "repetitions": "1|2|4|8|16|32", "pairs": "2", "sources": "2", "cases": "13",
        "decision_protocol": PROTOCOL, "bootstrap_seed": "20261025", "bootstrap_replicates": "1000",
    }
    write_rows(directory / "freeze.csv", [{"key": key, "value": value} for key, value in freeze.items()])
    shapes, queries, totals = [], [], []
    times = dict(zip(SUMMARY.ARMS, (100., 90., 80., 60., 95., 85., 75., 55., 80., 70., 120.)))
    for case, source in [(0, "source0"), (1, "source1"), (66, "four_batches_64"), (514, "four_batches_512"), (4098, "four_batches_4096")]:
        synthetic = source.startswith("four_batches_")
        group = "synthetic" if synthetic else "test"
        split = "synthetic" if synthetic else "development" if SUMMARY.source_hash(group, source) % 3 == 0 else "heldout"
        configurations = [(1024, "direct")] if synthetic else [(1024, "direct")] + [(16384, layout) for layout in SUMMARY.LAYOUTS]
        for page_rows, layout in configurations:
            pages = int(source.removeprefix("four_batches_")) if synthetic else 65536 // page_rows
            regions = 4 if synthetic else 0 if case == 1 else 2
            certified = pages if regions else 0
            shape = {
                "case": str(case), "split": split, "group": group, "source": source,
                "selector": "s", "value": "v", "page_rows": str(page_rows), "layout": layout,
                "rows": str(pages * page_rows), "pages": str(pages), "global_ordered": "false",
                "regions": str(regions), "certified_pages": str(certified), "multi_page_regions": str(regions),
                "longest_region": str(pages // regions if regions else 0), "selector_file_bytes": "10000",
                "value_file_bytes": "12000", "region_entry_bytes": str(regions * 16),
                "predicates": "1", "rule_fingerprint": FINGERPRINT,
            }
            for prefix, median in (("init_", 1000), ("region_", 100), ("prepare_", 10), ("region_prepare_", 10)):
                shape.update({prefix + "p25_ns": str(median * .9), prefix + "median_ns": str(median), prefix + "p75_ns": str(median * 1.1)})
            shapes.append(shape)
            for arm in SUMMARY.ARMS:
                median = times[arm]
                queries.append({
                    "case": str(case), "page_rows": str(page_rows), "layout": layout, "predicate": "0",
                    "requested": "0.001|0.010|0.100|0.500", "low": "1", "high": "10",
                    "expected_sum": "-55", "expected_count": "10", "arm": arm,
                    "p25_ns": str(median*.9), "median_ns": str(median), "p75_ns": str(median*1.1),
                    "sum": "-55", "matched": "10", "candidate_pages": str(pages), "skipped": "0",
                    "accepted": "0", "searched": "0", "translated": "0", "scanned": str(pages),
                    "rule_fingerprint": FINGERPRINT,
                })
            if layout == "direct":
                for repetitions in SUMMARY.REPETITIONS:
                    for arm in SUMMARY.TOTAL_ARMS:
                        median = 1000 + (100 if "regions" in arm else 0) + (10 if "prepared" in arm else 0) + repetitions*times[arm]
                        totals.append({
                            "case": str(case), "page_rows": str(page_rows), "layout": layout, "predicate": "0",
                            "repetitions": str(repetitions), "arm": arm, "p25_ns": str(median*.9),
                            "median_ns": str(median), "p75_ns": str(median*1.1), "sum": "-55", "matched": "10",
                            "rule_fingerprint": FINGERPRINT,
                        })
    for name, rows in (("shapes.csv", shapes), ("queries.csv", queries), ("totals.csv", totals)):
        write_rows(directory / name, rows)
    return shapes, queries, totals


class QueryOptimizationSummaryTests(unittest.TestCase):
    def test_source_partition_hash_matches_fnv1a_known_vector(self):
        self.assertEqual(SUMMARY.source_hash("tpch", "lineitem"), 0x20CA6864C1D47EA3)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.directory = Path(self.temporary.name)
        self.shapes, self.queries, self.totals = fixture(self.directory)

    def tearDown(self):
        self.temporary.cleanup()

    def test_complete_matrix_includes_zero_region_cases_and_measured_totals(self):
        result = SUMMARY.analyze(self.directory)
        self.assertEqual(result["shapes"], 13)
        self.assertEqual(result["natural_sources"], 2)
        self.assertEqual(result["query_cells"], len(self.queries))
        self.assertEqual(result["total_cells"], len(self.totals))
        self.assertEqual(sum(row["pairs_with_no_multi_page_regions"] for row in result["prevalence"] if row["split"] != "synthetic"), 2)
        self.assertFalse(result["automatic_promotion"])
        # Both same-shortcuts controls participate in measured total ratios.
        self.assertIn("regions_zonemapplus", {row["comparison"] for row in result["total_comparisons"]})
        self.assertTrue(result["engineering_decision"]["prepared_gate_passed"])
        SUMMARY.write_summary(self.directory, result)
        self.assertIn("teardown", (self.directory / "summary.md").read_text())

    def test_missing_strong_control_warm_cell_is_rejected(self):
        rows = [row for row in self.queries if not (row["case"] == "0" and row["page_rows"] == "1024" and row["arm"] == "zonemapplus_cost_prepared")]
        write_rows(self.directory / "queries.csv", rows)
        with self.assertRaisesRegex(ValueError, "incomplete warm"):
            SUMMARY.analyze(self.directory)

    def test_missing_total_and_duplicate_query_are_rejected(self):
        write_rows(self.directory / "totals.csv", self.totals[:-1])
        with self.assertRaisesRegex(ValueError, "incomplete end-to-end"):
            SUMMARY.analyze(self.directory)
        write_rows(self.directory / "totals.csv", self.totals)
        write_rows(self.directory / "queries.csv", self.queries + [self.queries[0]])
        with self.assertRaisesRegex(ValueError, "duplicate query"):
            SUMMARY.analyze(self.directory)

    def test_wrong_oracle_answer_is_rejected(self):
        rows = copy.deepcopy(self.queries)
        rows[0]["sum"] = "-54"
        write_rows(self.directory / "queries.csv", rows)
        with self.assertRaisesRegex(ValueError, "exact oracle"):
            SUMMARY.analyze(self.directory)

    def test_preparation_cannot_change_page_actions_even_with_exact_answers(self):
        rows = copy.deepcopy(self.queries)
        changed = next(row for row in rows if row["arm"] == "facts_prepared")
        changed["scanned"] = str(int(changed["scanned"]) - 1)
        changed["searched"] = "1"
        write_rows(self.directory / "queries.csv", rows)
        with self.assertRaisesRegex(ValueError, "preparation changed"):
            SUMMARY.analyze(self.directory)

    def test_cost_regressions_remain_in_gate_denominator(self):
        rows = copy.deepcopy(self.queries)
        for row in rows:
            if row["arm"] == "facts_cost_prepared":
                row.update(p25_ns="450", median_ns="500", p75_ns="550")
        write_rows(self.directory / "queries.csv", rows)
        report = SUMMARY.analyze(self.directory)
        decision = report["engineering_decision"]
        self.assertFalse(decision["cost_gate_passed"])
        self.assertEqual(decision["cost_heldout"]["queries"], decision["cost_heldout"]["regressions_over_20_percent"])

    def test_changed_fingerprint_and_nan_timing_are_rejected(self):
        rows = copy.deepcopy(self.queries)
        rows[0]["rule_fingerprint"] = "fedcba9876543210"
        write_rows(self.directory / "queries.csv", rows)
        with self.assertRaisesRegex(ValueError, "fingerprint"):
            SUMMARY.analyze(self.directory)
        rows = copy.deepcopy(self.queries)
        rows[0]["median_ns"] = "nan"
        write_rows(self.directory / "queries.csv", rows)
        with self.assertRaisesRegex(ValueError, "timing quartiles"):
            SUMMARY.analyze(self.directory)

    def test_incomplete_shape_and_wrong_region_bytes_are_rejected(self):
        write_rows(self.directory / "shapes.csv", self.shapes[:-1])
        with self.assertRaisesRegex(ValueError, "shape coverage"):
            SUMMARY.analyze(self.directory)
        rows = copy.deepcopy(self.shapes)
        rows[0]["region_entry_bytes"] = "0"
        write_rows(self.directory / "shapes.csv", rows)
        with self.assertRaisesRegex(ValueError, "payload accounting"):
            SUMMARY.analyze(self.directory)

    def test_source_level_bootstrap_is_deterministic_and_clusters_queries(self):
        values = [("sourceA", .5)]*30 + [("sourceB", 2.)]
        first = SUMMARY.ratio_summary(values)
        self.assertEqual(first, SUMMARY.ratio_summary(values))
        self.assertAlmostEqual(first["source_weighted_geometric_mean"], 1.)
        self.assertLess(first["geometric_mean"], .6)
        self.assertEqual(first["source_weighted_source_bootstrap_ci95"], [.5, 2.])

    def test_partial_freeze_and_changed_protocol_are_rejected(self):
        rows = SUMMARY.read_csv(self.directory / "freeze.csv")
        next(row for row in rows if row["key"] == "status")["value"] = "started"
        write_rows(self.directory / "freeze.csv", rows)
        with self.assertRaisesRegex(ValueError, "incomplete"):
            SUMMARY.analyze(self.directory)
        next(row for row in rows if row["key"] == "status")["value"] = "complete"
        next(row for row in rows if row["key"] == "bootstrap_seed")["value"] = "1"
        write_rows(self.directory / "freeze.csv", rows)
        with self.assertRaisesRegex(ValueError, "protocol"):
            SUMMARY.analyze(self.directory)


if __name__ == "__main__":
    unittest.main()
