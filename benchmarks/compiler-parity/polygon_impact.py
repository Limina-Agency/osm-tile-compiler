"""Rank valid, uniquely paired OMT polygon coverage differences on selected tiles.

This is a diagnostic sample. Symmetric-difference area measures changed coverage,
not visual similarity or whole-archive semantic parity. Polygon overlay and
Hausdorff costs are bounded by per-pair coordinate caps after tile decoding;
excluded cases are counted explicitly.
"""

import argparse
from collections import Counter, defaultdict
from contextlib import ExitStack
import hashlib
import importlib.util
import json
import math
from pathlib import Path

from shapely.geometry import shape
from shapely import get_num_coordinates

spec = importlib.util.spec_from_file_location("omt_compare", Path(__file__).with_name("compare.py"))
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


def select_coordinates(comparison_report, per_zoom=64, low_zoom_through=8, known=()):
    if per_zoom < 1:
        raise ValueError("per_zoom must be positive")
    mismatched = {tuple(row["tile"]) for row in comparison_report["differences"]}
    by_zoom = defaultdict(list)
    for xyz in mismatched:
        by_zoom[xyz[0]].append(xyz)
    selected = set(known)
    for zoom, coordinates in by_zoom.items():
        order = sorted(coordinates, key=lambda xyz: (hashlib.sha256(
            json.dumps(xyz).encode("utf-8")).digest(), xyz))
        selected.update(order if zoom <= low_zoom_through else order[:per_zoom])
    return sorted(selected)


def polygon_rows(archive, xyz):
    if xyz not in archive.tiles:
        return {}, [{"scope": "absent_tile", "coordinate": xyz}]
    decoder = comparison.validator.TileData(archive.tile(xyz), default_options={"y_coord_down": True})
    rows = defaultdict(list)
    errors = []
    for layer in decoder.tile.layers:
        for feature in layer.features:
            if feature.type != 3:
                continue
            key = (layer.name, feature.id if feature.HasField("id") else None, feature.type)
            row = {"extent": layer.extent, "geometry": None, "error": None}
            try:
                row["geometry"] = shape(decoder.parse_geometry(
                    feature.geometry, feature.type, layer.extent, True, None))
            except Exception as error:
                row["error"] = str(error)
                errors.append({"scope": "feature", "coordinate": xyz,
                               "layer": layer.name, "id": key[1], "error": str(error)})
            rows[key].append(row)
    return rows, errors


def feature_key_order(key):
    layer, fid, kind = key
    return layer, fid is None, fid if fid is not None else 0, kind


def validate_output_path(output, inputs):
    destination = output.resolve()
    if destination in {path.resolve() for path in inputs}:
        raise ValueError("Output path must differ from archive and comparison inputs")


def validate_comparison_source(report, native_hash, reference_hash):
    recorded = report.get("archive_sha256", {})
    if recorded.get("left") != native_hash or recorded.get("right") != reference_hash:
        raise ValueError("Comparison report is not bound to these native/reference archives")


def measure_pair(native, reference, max_coordinates=2000, hausdorff_cap=500):
    if native["geometry"] is None or reference["geometry"] is None:
        return {"status": "decode_error"}
    if native["extent"] != reference["extent"] or native["extent"] <= 0:
        return {"status": "extent_mismatch"}
    left, right = native["geometry"], reference["geometry"]
    if left.geom_type not in ("Polygon", "MultiPolygon") or right.geom_type not in (
        "Polygon", "MultiPolygon"):
        return {"status": "nonpolygon"}
    if left.is_empty or right.is_empty or not left.is_valid or not right.is_valid:
        return {"status": "invalid_or_empty", "native_valid": bool(left.is_valid),
                "reference_valid": bool(right.is_valid)}
    count = int(get_num_coordinates(left) + get_num_coordinates(right))
    if max_coordinates and count > max_coordinates:
        return {"status": "cost_cap", "combined_coordinates": count}
    try:
        difference = left.symmetric_difference(right).area
        union = left.union(right).area
        pixel_scale = 256.0 / native["extent"]
        area_scale = pixel_scale * pixel_scale
        result = {"status": "measured", "combined_coordinates": count,
                  "symmetric_difference_px2": difference * area_scale,
                  "union_px2": union * area_scale,
                  "coverage_disagreement_fraction": difference / union if union else None,
                  "native_area_px2": left.area * area_scale,
                  "reference_area_px2": right.area * area_scale,
                  "centroid_displacement_px": left.centroid.distance(right.centroid) * pixel_scale,
                  "native_bounds_px": [v * pixel_scale for v in left.bounds],
                  "reference_bounds_px": [v * pixel_scale for v in right.bounds]}
        if count <= hausdorff_cap:
            result["hausdorff_px"] = left.hausdorff_distance(right) * pixel_scale
        else:
            result["hausdorff_px"] = None
        if not all(math.isfinite(value) for value in (
            result["symmetric_difference_px2"], result["union_px2"],
            result["centroid_displacement_px"])):
            return {"status": "nonfinite_metric"}
        return result
    except Exception as error:
        return {"status": "overlay_error", "error": str(error)}


def audit(native_path, reference_path, coordinates, max_coordinates=2000, hausdorff_cap=500):
    if max_coordinates < 1 or hausdorff_cap < 0:
        raise ValueError("max_coordinates must be positive and hausdorff_cap nonnegative")
    counts = Counter()
    layer_status = Counter()
    measured = []
    exclusions = []
    with ExitStack() as stack:
        native = stack.enter_context(comparison.open_archive(native_path))
        reference = stack.enter_context(comparison.open_archive(reference_path))
        for xyz in coordinates:
            try:
                left, left_errors = polygon_rows(native, xyz)
                right, right_errors = polygon_rows(reference, xyz)
            except Exception as error:
                counts["tile_decode_error"] += 1
                exclusions.append({"coordinate": xyz, "status": "tile_decode_error",
                                   "error": str(error)})
                continue
            counts["feature_decode_errors"] += sum(
                error["scope"] == "feature" for error in left_errors + right_errors)
            for side, errors in (("native", left_errors), ("reference", right_errors)):
                for error in errors:
                    counts[error["scope"]] += 1
                    if len(exclusions) < 200:
                        exclusions.append(dict(error, side=side, status=error["scope"]))
            for key in sorted(set(left) | set(right), key=feature_key_order):
                layer, fid, kind = key
                l, r = left.get(key, []), right.get(key, [])
                context = {"coordinate": xyz, "layer": layer, "id": fid, "type": kind}
                if fid is None or len(l) > 1 or len(r) > 1:
                    status = "ambiguous_id"
                elif not l or not r:
                    status = "unmatched"
                else:
                    metric = measure_pair(l[0], r[0], max_coordinates, hausdorff_cap)
                    status = metric["status"]
                    context.update(metric)
                counts[status] += 1
                layer_status[layer, status] += 1
                if status == "measured":
                    if context["symmetric_difference_px2"] > 0:
                        measured.append(context)
                elif len(exclusions) < 200:
                    exclusions.append(dict(context, status=status))
    measured.sort(key=lambda row: (-row["symmetric_difference_px2"],
                                   -row["coverage_disagreement_fraction"],
                                   row["coordinate"], row["layer"], row["id"]))
    by_layer = defaultdict(list)
    by_zoom = defaultdict(list)
    for row in measured:
        if len(by_layer[row["layer"]]) < 10:
            by_layer[row["layer"]].append(row)
        if len(by_zoom[row["coordinate"][0]]) < 10:
            by_zoom[row["coordinate"][0]].append(row)
    return {"format_version": 1, "scope": "Deterministic selected tiles only; unique polygon ID pairs, valid shapes, bounded overlay after decode. Pixel units refer to 256 px tile display. Invalid, unmatched, ambiguous, over-cap and decode errors are excluded, not treated as equal.",
            "archive_sha256": {"native": comparison.digest(native_path),
                               "reference": comparison.digest(reference_path)},
            "coordinates": coordinates, "counts": dict(counts),
            "different_measured_pairs": len(measured),
            "status_by_layer": [{"layer": layer, "status": status, "count": count}
                                for (layer, status), count in sorted(layer_status.items())],
            "top_impact": measured[:100], "top_by_layer": dict(by_layer),
            "top_by_zoom": dict(by_zoom),
            "exclusion_examples": exclusions}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("native", type=Path)
    parser.add_argument("reference", type=Path)
    parser.add_argument("--comparison-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--per-zoom", type=int, default=64)
    parser.add_argument("--low-zoom-through", type=int, default=8)
    parser.add_argument("--max-coordinates", type=int, default=2000)
    parser.add_argument("--hausdorff-cap", type=int, default=500)
    args = parser.parse_args()
    validate_output_path(args.output, (args.native, args.reference, args.comparison_report))
    report = json.loads(args.comparison_report.read_text(encoding="utf-8-sig"))
    validate_comparison_source(report, comparison.digest(args.native),
                               comparison.digest(args.reference))
    coordinates = select_coordinates(report, args.per_zoom, args.low_zoom_through,
                                     known=((7, 70, 43),))
    output = audit(args.native, args.reference, coordinates, args.max_coordinates,
                   args.hausdorff_cap)
    output["coordinate_source_sha256"] = comparison.digest(args.comparison_report)
    args.output.write_text(json.dumps(output, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    print("selected tiles", len(coordinates), "measured pairs", output["counts"].get("measured", 0))


if __name__ == "__main__":
    main()
