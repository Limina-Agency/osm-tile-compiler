import importlib.util
from collections import Counter, defaultdict
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "all_omt", Path(__file__).with_name("diagnose_all_omt.py")
)
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class PairTests(unittest.TestCase):
    def test_unique_pair_separates_wire_type_value_and_geometry(self):
        key = ("transportation", 42, 2)
        left = {
            key: [
                {
                    "attrs": {"a": ["int_value", 7], "b": ["string_value", "x"]},
                    "geometry": "a",
                    "extent": 4096,
                }
            ]
        }
        right = {
            key: [
                {
                    "attrs": {"a": ["sint_value", 7], "b": ["string_value", "y"]},
                    "geometry": "b",
                    "extent": 4096,
                }
            ]
        }
        counts = Counter()
        examples = defaultdict(list)
        self.assertEqual(m.pair(left, right, (14, 1, 1), counts, examples), [])
        self.assertEqual(counts["transportation", "shared_unique_keys"], 1)
        self.assertEqual(counts["transportation", "typed_attribute"], 1)
        self.assertEqual(counts["transportation", "attribute"], 1)
        self.assertEqual(counts["transportation", "geometry"], 1)

    def test_duplicate_and_missing_ids_never_form_false_pairs(self):
        key = ("water", 9, 3)
        none = ("water", None, 3)
        only = ("water", 10, 3)
        row = {"attrs": {}, "geometry": "g", "extent": 4096}
        left = {key: [row, row], none: [row], only: [row]}
        right = {key: [row], none: [row]}
        counts = Counter()
        examples = defaultdict(list)
        collisions = m.pair(left, right, (9, 1, 1), counts, examples)
        self.assertEqual(len(collisions), 2)
        self.assertEqual(counts["water", "ambiguous_keys"], 2)
        self.assertEqual(counts["water", "native_only"], 1)
        self.assertEqual(counts["water", "shared_unique_keys"], 0)

    def test_failed_geometry_decode_is_inconclusive(self):
        key = ("waterway", 17, 2)
        left = {key: [{"attrs": {}, "geometry": None, "extent": 4096}]}
        right = {key: [{"attrs": {}, "geometry": "decoded", "extent": 4096}]}
        counts = Counter()
        examples = defaultdict(list)

        self.assertEqual(m.pair(left, right, (14, 1, 1), counts, examples), [])
        self.assertEqual(counts["waterway", "shared_unique_keys"], 1)
        self.assertEqual(counts["waterway", "geometry_pair_inconclusive"], 1)
        self.assertEqual(counts["waterway", "geometry"], 0)
        self.assertFalse(
            examples["waterway", "geometry_pair_inconclusive"][0]["native_decoded"]
        )


if __name__ == "__main__":
    unittest.main()
