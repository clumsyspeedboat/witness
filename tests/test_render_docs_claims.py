"""Numerical prose must follow reruns without changing unrelated text."""

import contextlib
import csv
import importlib.util
import io
from pathlib import Path
import shutil
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "render_docs_claims", ROOT / "experiments/render_docs_claims.py"
)
RENDERER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RENDERER)


class RenderDocsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in RENDERER.RULES:
            target = self.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, target)
        with (ROOT / "experiments/results/claim_manifest.csv").open() as source:
            self.claims = {row["claim"]: row["value"] for row in csv.DictReader(source)}

    def render(self):
        with contextlib.redirect_stdout(io.StringIO()):
            RENDERER.render_docs(self.root, self.claims)

    def contents(self):
        return {name: (self.root / name).read_text() for name in RENDERER.RULES}

    def test_repeated_render_is_idempotent(self):
        self.render()
        first = self.contents()
        self.render()
        self.assertEqual(first, self.contents())

    def test_changed_claims_update_every_context_and_preserve_wrapping(self):
        before = self.contents()
        self.claims["WitSourceDirectBoundaryCiHigh"] = "0.19"
        self.claims["WitColdPageOverFull"] = "1.47"
        self.claims["WitPagedFactsOverZonemapGeo"] = "0.89"
        self.render()
        after = self.contents()
        self.assertIn("[0.08, 0.19]", after["docs/LIMITATIONS.md"])
        self.assertIn("[0.08, 0.19]", after["docs/ENCODINGS.md"])
        self.assertIn("1.47x a full\nread", after["docs/WALKTHROUGH.md"])
        self.assertIn("**0.89x** the time (geometric mean)", after["README.md"])
        for name in before:
            self.assertEqual(before[name].count("\n"), after[name].count("\n"))

    def test_missing_late_phrase_prevents_all_writes(self):
        path = self.root / "docs/WALKTHROUGH.md"
        path.write_text(path.read_text().replace("per-page closure", "changed wording"))
        before = self.contents()
        self.claims["WitPagedFactsOverZonemapGeo"] = "0.89"
        with self.assertRaises(ValueError):
            self.render()
        self.assertEqual(before, self.contents())

    def test_missing_claim_is_rejected(self):
        before = self.contents()
        del self.claims["WitAccessPremium"]
        with self.assertRaises(KeyError):
            self.render()
        self.assertEqual(before, self.contents())


if __name__ == "__main__":
    unittest.main()
