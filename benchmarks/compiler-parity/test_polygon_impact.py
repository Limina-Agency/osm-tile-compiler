import unittest
import importlib.util
from pathlib import Path
from tempfile import TemporaryDirectory

from shapely.geometry import Polygon

spec = importlib.util.spec_from_file_location("polygon_impact", Path(__file__).with_name("polygon_impact.py"))
polygon_impact = importlib.util.module_from_spec(spec)
spec.loader.exec_module(polygon_impact)


def row(shape, extent=4096):
    return {"geometry": shape, "extent": extent}


class PolygonImpactTests(unittest.TestCase):
    def test_coverage_area_uses_screen_pixels(self):
        left = Polygon([(0, 0), (32, 0), (32, 16), (0, 16)])
        right = Polygon([(16, 0), (48, 0), (48, 16), (16, 16)])
        metric = polygon_impact.measure_pair(row(left), row(right))
        self.assertEqual(metric["status"], "measured")
        self.assertAlmostEqual(metric["symmetric_difference_px2"], 2.0)
        self.assertAlmostEqual(metric["union_px2"], 3.0)
        self.assertAlmostEqual(metric["coverage_disagreement_fraction"], 2 / 3)
        self.assertAlmostEqual(metric["centroid_displacement_px"], 1.0)

    def test_invalid_and_cost_capped_are_not_measured(self):
        bowtie = Polygon([(0, 0), (10, 10), (0, 10), (10, 0)])
        square = Polygon([(0, 0), (10, 0), (10, 10), (0, 10)])
        self.assertEqual(polygon_impact.measure_pair(row(bowtie), row(square))["status"],
                         "invalid_or_empty")
        self.assertEqual(polygon_impact.measure_pair(row(square), row(square), max_coordinates=5)["status"],
                         "cost_cap")
        self.assertEqual(polygon_impact.measure_pair(row(square, 4096), row(square, 2048))["status"],
                         "extent_mismatch")

    def test_selection_includes_low_zoom_and_known(self):
        report = {"differences": [{"tile": [7, 1, 2]}, {"tile": [13, 3, 4]},
                                  {"tile": [13, 4, 4]}, {"tile": [13, 5, 4]}]}
        result = polygon_impact.select_coordinates(report, per_zoom=1,
                                                    known=((7, 70, 43),))
        self.assertEqual(len(result), 3)
        self.assertIn((7, 1, 2), result)
        self.assertIn((7, 70, 43), result)
        self.assertEqual(sum(z == 13 for z, _, _ in result), 1)

    def test_null_ids_have_stable_sort_order(self):
        keys = [("landcover", None, 3), ("landcover", 5, 3),
                ("landcover", 1, 3)]
        self.assertEqual(sorted(keys, key=polygon_impact.feature_key_order),
                         [("landcover", 1, 3), ("landcover", 5, 3),
                          ("landcover", None, 3)])

    def test_output_cannot_overwrite_any_input(self):
        with TemporaryDirectory() as directory:
            source = Path(directory) / "archive.pmtiles"
            report = Path(directory) / "comparison.json"
            source.write_bytes(b"x")
            report.write_text("{}")
            with self.assertRaises(ValueError):
                polygon_impact.validate_output_path(source, (source, report))
            with self.assertRaises(ValueError):
                polygon_impact.validate_output_path(report, (source, report))
            polygon_impact.validate_output_path(Path(directory) / "output.json",
                                                (source, report))

    def test_audit_rejects_unbounded_overlay(self):
        with self.assertRaises(ValueError):
            polygon_impact.audit(Path("missing"), Path("missing"), [], max_coordinates=0)

    def test_absent_tile_is_explicit_exclusion(self):
        class EmptyArchive:
            tiles = set()

        rows, errors = polygon_impact.polygon_rows(EmptyArchive(), (8, 100, 60))
        self.assertEqual(rows, {})
        self.assertEqual(errors, [{"scope": "absent_tile", "coordinate": (8, 100, 60)}])

    def test_comparison_coordinates_must_match_archive_hashes(self):
        report = {"archive_sha256": {"left": "native-hash", "right": "reference-hash"}}
        polygon_impact.validate_comparison_source(report, "native-hash", "reference-hash")
        with self.assertRaises(ValueError):
            polygon_impact.validate_comparison_source(report, "other-native", "reference-hash")


if __name__ == "__main__":
    unittest.main()
