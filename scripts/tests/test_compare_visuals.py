import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from PIL import Image


SCRIPT = Path(__file__).parents[1] / "compare-visuals.py"
SPEC = importlib.util.spec_from_file_location("compare_visuals", SCRIPT)
assert SPEC and SPEC.loader
compare_visuals = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = compare_visuals
SPEC.loader.exec_module(compare_visuals)


class SelectionTests(unittest.TestCase):
    def test_page_parsing_and_even_sampling(self):
        self.assertEqual(compare_visuals.parse_pages("1,3-4,last", 10), [1, 3, 4, 10])
        self.assertEqual(compare_visuals.evenly_spaced(10, 5), [1, 3, 6, 8, 10])
        self.assertEqual(compare_visuals.cap_evenly([1, 8, 9, 11, 20], 3), [1, 9, 20])
        self.assertEqual(compare_visuals.cap_evenly([1, 2], 0), [])

    def test_auto_selection_combines_reasons(self):
        with mock.patch.object(
            compare_visuals,
            "qpdf_page_signals",
            return_value=({1: 10, 4: 500, 7: 200}, [4, 9]),
        ):
            reasons = compare_visuals.auto_page_reasons(
                Path("input.pdf"), 10, sample_pages=3, raster_pages=2, max_signature_pages=5
            )
        self.assertEqual(reasons[1], {"uniform"})
        self.assertEqual(reasons[4], {"raster-heavy", "signature"})
        self.assertEqual(reasons[7], {"raster-heavy"})
        self.assertEqual(reasons[10], {"uniform"})
        self.assertEqual(reasons[9], {"signature"})


class MetricTests(unittest.TestCase):
    def test_identical_and_changed_images(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            reference = root / "reference.png"
            identical = root / "identical.png"
            changed = root / "changed.png"
            Image.new("RGB", (8, 8), "white").save(reference)
            Image.new("RGB", (8, 8), "white").save(identical)
            Image.new("RGB", (8, 8), "black").save(changed)

            equal = compare_visuals.compare_page(
                "x.pdf", 1, "explicit", reference, identical, "original", 8, root
            )
            self.assertEqual(equal.status, "equal")
            self.assertEqual(equal.psnr_db, float("inf"))

            different = compare_visuals.compare_page(
                "x.pdf", 2, "explicit", reference, changed, "original", 8, root
            )
            self.assertEqual(different.status, "changed")
            self.assertEqual(different.changed_pct, 100.0)
            self.assertEqual(different.psnr_db, 0.0)


if __name__ == "__main__":
    unittest.main()
