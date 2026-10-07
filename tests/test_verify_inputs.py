"""Regression checks for the complete-corpus reproduction gate."""

import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "experiments/eval/scripts/verify_inputs.py"
SPEC = importlib.util.spec_from_file_location("verify_inputs", SCRIPT)
inputs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(inputs)


class VerifyInputsTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        self.data = self.root / "data"
        self.column = self.data / "census/tpch/lineitem__c0_key.csv"
        self.column.parent.mkdir(parents=True)
        self.column.write_text("1\n2\n3\n")
        self.manifest = self.root / "INPUTS.sha256"
        self.manifest.write_text(
            f"{inputs.digest(self.column)}  {self.column.relative_to(self.data)}\n"
        )

    def test_unchanged_corpus_passes(self):
        self.assertEqual(inputs.verify(self.data, self.manifest), 1)

    def test_modified_column_fails(self):
        self.column.write_text("1\n2\n4\n")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            inputs.verify(self.data, self.manifest)

    def test_missing_column_fails(self):
        self.column.unlink()
        with self.assertRaisesRegex(ValueError, "missing input"):
            inputs.verify(self.data, self.manifest)

    def test_extra_discovered_column_fails(self):
        self.column.with_name("lineitem__c1_value.csv").write_text("4\n5\n6\n")
        with self.assertRaisesRegex(ValueError, "unlisted input"):
            inputs.verify(self.data, self.manifest)


if __name__ == "__main__":
    unittest.main()
