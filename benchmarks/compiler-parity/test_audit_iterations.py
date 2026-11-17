"""Focused transition-audit tests with small decoded tile fixtures."""

from contextlib import nullcontext
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch
from shapely.geometry import Polygon


spec = importlib.util.spec_from_file_location(
    "audit_iterations", Path(__file__).with_name("audit_iterations.py")
)
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


def feature(geometry="g", attrs=None):
    return {
        "geometry": geometry,
        "attrs": {"rank": ["int_value", 7]} if attrs is None else attrs,
        "extent": 4096,
    }


class AuditTests(unittest.TestCase):
    def run_fixture(self, rows, errors=None, polygon_area=False):
        xyz = (14, 1, 1)
        errors = errors or {}

        def decode(_archive, _xyz, side, _raw, reported):
            reported.extend(errors.get(side, []))
            return rows[side]

        with (
            patch.object(audit.helper.diag, "coordinates_from", return_value=[xyz]),
            patch.object(
                audit.helper.comparison,
                "open_archive",
                side_effect=lambda _: nullcontext(object()),
            ),
            patch.object(
                audit.helper.comparison, "digest", side_effect=lambda path: str(path)
            ),
            patch.object(audit.helper, "decode_tile", side_effect=decode),
        ):
            return audit.audit(
                Path("baseline.pmtiles"),
                Path("candidate.pmtiles"),
                Path("reference.pmtiles"),
                Path("coords.json"),
                Path("original.json"),
                Path("repeat.pmtiles"),
                polygon_area=polygon_area,
            )

    def test_removed_extra_is_absent_but_removed_reference_is_unmatched(self):
        extra = ("landcover", 1, 3)
        missing = ("landcover", 2, 3)
        report = self.run_fixture(
            {
                "baseline": {extra: [feature()], missing: [feature()]},
                "candidate": {},
                "reference": {missing: [feature()]},
            }
        )
        changed = {row["id"]: row for row in report["changed_keys"]}
        self.assertEqual(changed[1]["membership_category"], "removed_native_only")
        self.assertEqual(
            changed[1]["geometry_status"], {"before": "unmatched", "after": "absent"}
        )
        self.assertEqual(changed[2]["membership_category"], "new_reference_only")
        self.assertEqual(
            changed[2]["geometry_status"], {"before": "equal", "after": "unmatched"}
        )
        self.assertTrue(report["candidate_repeats_identical"] is False)

    def test_unique_geometry_and_typed_attribute_regressions(self):
        key = ("transportation", 3, 2)
        report = self.run_fixture(
            {
                "baseline": {key: [feature()]},
                "candidate": {key: [feature("shifted", {"rank": ["sint_value", 7]})]},
                "reference": {key: [feature()]},
            }
        )
        row = report["changed_keys"][0]
        self.assertEqual(
            row["geometry_status"], {"before": "equal", "after": "different"}
        )
        self.assertEqual(row["attrs_status"], {"before": "equal", "after": "different"})
        self.assertTrue(row["native_value_changed"]["geometry"])
        self.assertTrue(row["native_value_changed"]["attrs"])

    def test_decode_failure_duplicate_and_validity_are_separate(self):
        decode = ("water", 4, 3)
        duplicate = ("water", 5, 3)
        validity = ("water", 6, 3)
        report = self.run_fixture(
            {
                "baseline": {
                    decode: [feature()],
                    duplicate: [feature(), feature()],
                    validity: [feature()],
                },
                "candidate": {
                    decode: [feature(None)],
                    duplicate: [feature()],
                    validity: [feature()],
                },
                "reference": {
                    decode: [feature()],
                    duplicate: [feature()],
                    validity: [feature()],
                },
            },
            {
                "baseline": [
                    {"scope": "geometry_validity", "layer": "water", "id": 6, "type": 3}
                ],
                "candidate": [
                    {"scope": "geometry", "layer": "water", "id": 4, "type": 3}
                ],
            },
        )
        changed = {row["id"]: row for row in report["changed_keys"]}
        self.assertEqual(changed[4]["geometry_status"]["after"], "inconclusive")
        self.assertEqual(changed[5]["geometry_status"]["before"], "ambiguous")
        self.assertTrue(changed[6]["validity"]["baseline"])
        self.assertFalse(changed[6]["validity"]["candidate"])
        self.assertEqual(
            {e["scope"] for e in report["errors"]}, {"geometry", "geometry_validity"}
        )

    def test_content_multiset_can_match_when_ids_do_not(self):
        report = self.run_fixture(
            {
                "baseline": {("housenumber", 7, 1): [feature()]},
                "candidate": {("housenumber", 8, 1): [feature()]},
                "reference": {("housenumber", 9, 1): [feature()]},
            }
        )
        row = next(
            row
            for row in report["id_agnostic_content"]
            if row["layer"] == "housenumber" and row["side"] == "candidate"
        )
        self.assertTrue(row["decoded_content_equal"])
        self.assertTrue(row["complete_decoded_coverage"])
        self.assertEqual(row["matched_without_id"], 1)
        self.assertTrue(
            any(
                item["geometry_status"]["after"] == "unmatched"
                for item in report["changed_keys"]
            )
        )

    def test_removed_idless_extra_is_absent(self):
        key = ("landcover", None, 3)
        report = self.run_fixture(
            {
                "baseline": {key: [feature()]},
                "candidate": {},
                "reference": {},
            }
        )
        row = report["changed_keys"][0]
        self.assertEqual(row["membership_category"], "removed_native_only")
        self.assertEqual(row["geometry_status"]["before"], "ambiguous")
        self.assertEqual(row["geometry_status"]["after"], "absent")

    def test_whole_tile_decode_failure_never_claims_content_equality(self):
        key = ("housenumber", 10, 1)
        report = self.run_fixture(
            {
                "baseline": {key: [feature()]},
                "candidate": {},
                "reference": {},
            },
            {
                "candidate": [{"scope": "tile", "error": "bad protobuf"}],
                "reference": [{"scope": "tile", "error": "bad protobuf"}],
            },
        )
        content = next(
            row
            for row in report["id_agnostic_content"]
            if row["layer"] == "housenumber" and row["side"] == "candidate"
        )
        self.assertEqual(content["tile_or_layer_decode_errors"], 2)
        self.assertFalse(content["complete_decoded_coverage"])
        self.assertIsNone(content["decoded_content_equal"])

    def test_absent_tile_does_not_claim_complete_decoded_content(self):
        key = ("housenumber", 11, 1)
        report = self.run_fixture(
            {
                "baseline": {key: [feature()]},
                "candidate": {},
                "reference": {},
            },
            {
                "candidate": [{"scope": "absent_tile"}],
                "reference": [{"scope": "absent_tile"}],
            },
        )
        content = next(
            row
            for row in report["id_agnostic_content"]
            if row["layer"] == "housenumber" and row["side"] == "candidate"
        )
        self.assertEqual(content["absent_tile_inputs"], 2)
        self.assertEqual(content["tile_or_layer_decode_errors"], 0)
        self.assertFalse(content["complete_decoded_coverage"])
        self.assertIsNone(content["decoded_content_equal"])

    def test_polygon_area_reports_closer_and_binds_hashes(self):
        key = ("landcover", 12, 3)
        reference = Polygon([(0, 0), (10, 0), (10, 10), (0, 10)]).wkb_hex
        baseline = Polygon([(2, 0), (12, 0), (12, 10), (2, 10)]).wkb_hex
        candidate = Polygon([(1, 0), (11, 0), (11, 10), (1, 10)]).wkb_hex
        report = self.run_fixture(
            {
                "baseline": {key: [feature(baseline)]},
                "candidate": {key: [feature(candidate)]},
                "reference": {key: [feature(reference)]},
            },
            polygon_area=True,
        )
        area = report["polygon_area_audit"]
        self.assertEqual(area["counts"], {"closer": 1})
        self.assertAlmostEqual(area["rows"][0]["before_area_mvt2"], 40)
        self.assertAlmostEqual(area["rows"][0]["after_area_mvt2"], 20)
        self.assertEqual(area["archive_sha256"], report["archive_sha256"])
        self.assertEqual(
            area["coordinate_source_sha256"], report["coordinate_source"]["sha256"]
        )

    def test_polygon_area_reports_worse_and_excludes_invalid(self):
        square = Polygon([(0, 0), (10, 0), (10, 10), (0, 10)]).wkb_hex
        near = Polygon([(1, 0), (11, 0), (11, 10), (1, 10)]).wkb_hex
        far = Polygon([(2, 0), (12, 0), (12, 10), (2, 10)]).wkb_hex
        rows = {
            "baseline": [feature(near)],
            "candidate": [feature(far)],
            "reference": [feature(square)],
        }
        worse = audit.polygon_area_change(rows, {side: False for side in rows})
        self.assertEqual(worse["status"], "worse")
        self.assertAlmostEqual(worse["delta_area_mvt2"], 20)

        bowtie = Polygon([(0, 0), (2, 2), (0, 2), (2, 0)]).wkb_hex
        rows["candidate"] = [feature(bowtie)]
        self.assertEqual(
            audit.polygon_area_change(rows, {side: False for side in rows})["status"],
            "excluded_invalid",
        )


if __name__ == "__main__":
    unittest.main()
