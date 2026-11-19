import importlib.util
import unittest
from pathlib import Path

import shapely
from shapely.geometry import LineString, MultiPolygon, Polygon, box

spec = importlib.util.spec_from_file_location("pairdiff", Path(__file__).with_name("pairdiff.py"))
pairdiff = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pairdiff)


def row(geometry, attrs=None):
    return {"geometry": geometry, "attrs": attrs or {}, "extent": 4096,
            "canonical": pairdiff.canonical(geometry)}


class PairdiffTests(unittest.TestCase):
    def test_ring_start_and_winding_do_not_count(self):
        a = Polygon([(0, 0), (10, 0), (10, 10), (0, 10)])
        b = Polygon([(10, 10), (10, 0), (0, 0), (0, 10)])
        self.assertEqual(pairdiff.canonical(a), pairdiff.canonical(b))

    def test_line_direction_counts(self):
        self.assertNotEqual(pairdiff.canonical(LineString([(0, 0), (1, 1)])),
                            pairdiff.canonical(LineString([(1, 1), (0, 0)])))

    def test_pairs_identical_rows_before_order(self):
        a, b, c = box(0, 0, 1, 1), box(5, 5, 6, 6), box(9, 9, 10, 10)
        pairs, left, right = pairdiff.pair_rows([row(a), row(b)], [row(b), row(c), row(a)])
        self.assertEqual(len(pairs), 2)
        self.assertTrue(all(l["canonical"] == r["canonical"] for l, r in pairs))
        self.assertEqual([r["canonical"] for r in right], [pairdiff.canonical(c)])
        self.assertEqual(left, [])

    def test_bar_in_256_pixel_units(self):
        # 4096 units per tile: 16 units is one pixel.
        reference = box(0, 0, 4000, 4000)
        inside = pairdiff.measure_polygons(box(0, 0, 4000, 4008), reference, 4096)
        self.assertTrue(inside["within_bar"])
        self.assertAlmostEqual(inside["hausdorff_px"], 0.5)
        too_far = pairdiff.measure_polygons(box(0, 0, 4000, 4016), reference, 4096)
        self.assertFalse(too_far["within_bar"])
        too_much = pairdiff.measure_polygons(box(0, 0, 100, 102), box(0, 0, 100, 100), 4096)
        self.assertLess(too_much["hausdorff_px"], 1)
        self.assertFalse(too_much["within_bar"])

    def test_indexed_hausdorff_matches_geos(self):
        star = shapely.buffer(shapely.Point(2000, 2000), 1500, quad_segs=600)
        holed = Polygon(star.exterior.coords, [box(1800, 1800, 2200, 2200).exterior.coords])
        moved = shapely.affinity.translate(shapely.affinity.rotate(holed, 0.3), 3, -2)
        moved = MultiPolygon([moved, box(0, 0, 50, 50)])
        self.assertGreater(shapely.get_num_coordinates(holed) + shapely.get_num_coordinates(moved),
                           pairdiff.INDEXED_HAUSDORFF_FROM)
        expected = holed.hausdorff_distance(moved)
        self.assertAlmostEqual(pairdiff.hausdorff(holed, moved), expected, places=6)

    def test_failures_and_acceptance(self):
        rows = [{"kind": "polygon_geometry", "within_bar": True, "layer": "building"},
                {"kind": "polygon_geometry", "within_bar": False, "layer": "building", "tile": [13, 1, 2], "id": 7},
                {"kind": "attributes", "layer": "poi", "tile": [14, 1, 1], "id": 3}]
        accepted = [{"layer": "poi", "kind": "attributes", "reason": "Han romanisation"}]
        layers, failures, accepted_rows = pairdiff.summarise(rows, accepted)
        self.assertEqual(layers["building"]["polygon_within_bar"], 1)
        self.assertEqual([f["id"] for f in failures], [7])
        self.assertEqual(accepted_rows[0]["accepted"], "Han romanisation")

    def test_invalid_only_in_native_reads_every_tile(self):
        report = {"geometry_validity_issues": [
            {"side": "left", "tile": [14, 1, 1], "layer": "building", "feature_id": 4, "error": "Self-intersection"},
            {"side": "right", "tile": [14, 1, 1], "layer": "building", "feature_id": 4, "error": "Self-intersection"},
            {"side": "left", "tile": [9, 2, 3], "layer": "landcover", "feature_id": 22104720, "error": "Hole lies outside shell"},
            {"side": "right", "tile": [10, 5, 5], "layer": "landuse", "feature_id": 8, "error": "Self-intersection"}]}
        rows = pairdiff.invalid_only_in_native(report)
        self.assertEqual([(r["tile"], r["id"]) for r in rows], [([9, 2, 3], 22104720)])


if __name__ == "__main__":
    unittest.main()
