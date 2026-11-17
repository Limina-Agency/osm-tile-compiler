"""Audit two OMT archive iterations against one reference on fixed coordinates."""

import argparse
from collections import Counter
from contextlib import ExitStack
import hashlib
import importlib.util
import json
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "all_omt", Path(__file__).with_name("diagnose_all_omt.py")
)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


def status(items, reference, fid, aspect):
    if not items and not reference:
        return "absent"
    if fid is None or len(items) > 1 or len(reference) > 1:
        return "ambiguous"
    if not items or not reference:
        return "unmatched"
    first, second = items[0], reference[0]
    if aspect == "geometry":
        if first["geometry"] is None or second["geometry"] is None:
            return "inconclusive"
        equal = (
            first["extent"] == second["extent"]
            and first["geometry"] == second["geometry"]
        )
    else:
        if first["attrs"] is None or second["attrs"] is None:
            return "inconclusive"
        equal = first["attrs"] == second["attrs"]
    return "equal" if equal else "different"


def membership_category(old, new, reference):
    if old == new:
        return None
    if old == 0 and new == 1 and reference == 0:
        return "new_native_only"
    if old == 1 and new == 0 and reference == 0:
        return "removed_native_only"
    if old == 1 and new == 0 and reference == 1:
        return "new_reference_only"
    if old == 0 and new == 1 and reference == 1:
        return "resolved_reference_only"
    return "other_count_change"


def signature(row, aspect):
    if len(row) != 1:
        return None
    value = row[0][aspect]
    if value is None:
        return None
    if aspect == "geometry":
        value = [row[0]["extent"], value]
    encoded = json.dumps(value, sort_keys=True, ensure_ascii=False).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def polygon_area_change(rows, validity, max_coordinates=2000):
    """Compare valid polygon coverage, without treating area as visual parity."""
    from shapely import from_wkb, get_num_coordinates

    if any(len(rows[side]) != 1 for side in ("baseline", "candidate", "reference")):
        return {"status": "excluded_inconclusive"}
    if any(validity.values()):
        return {"status": "excluded_invalid"}
    features = {side: rows[side][0] for side in rows}
    if any(feature["geometry"] is None for feature in features.values()):
        return {"status": "excluded_inconclusive"}
    if len({feature["extent"] for feature in features.values()}) != 1:
        return {"status": "excluded_extent_mismatch"}
    try:
        shapes = {
            side: from_wkb(bytes.fromhex(feature["geometry"]))
            for side, feature in features.items()
        }
        if any(
            shape.geom_type not in ("Polygon", "MultiPolygon")
            for shape in shapes.values()
        ):
            return {"status": "excluded_nonpolygon"}
        if any(not shape.is_valid for shape in shapes.values()):
            return {"status": "excluded_invalid"}
        coordinates = sum(int(get_num_coordinates(shape)) for shape in shapes.values())
        if max_coordinates and coordinates > max_coordinates:
            return {"status": "excluded_cost_cap", "combined_coordinates": coordinates}
        before = shapes["baseline"].symmetric_difference(shapes["reference"]).area
        after = shapes["candidate"].symmetric_difference(shapes["reference"]).area
        delta = after - before
        classification = (
            "closer" if delta < -1e-8 else "worse" if delta > 1e-8 else "same"
        )
        return {
            "status": classification,
            "before_area_mvt2": before,
            "after_area_mvt2": after,
            "delta_area_mvt2": delta,
            "combined_coordinates": coordinates,
        }
    except Exception as exc:
        return {"status": "error", "error": str(exc)}


def audit(
    baseline,
    candidate,
    reference,
    coordinate_source,
    original_source=None,
    repeat_archive=None,
    polygon_area=False,
    max_polygon_coordinates=2000,
):
    if max_polygon_coordinates < 0:
        raise ValueError("Maximum polygon coordinates must be nonnegative")
    coordinates = helper.diag.coordinates_from(coordinate_source)
    original = (
        set(helper.diag.coordinates_from(original_source)) if original_source else set()
    )
    if not original <= set(coordinates):
        raise ValueError(
            "Original coordinates must be a subset of selected coordinates"
        )
    archives = {
        "baseline": baseline,
        "candidate": candidate,
        "reference": reference,
    }

    totals = Counter()
    changes = []
    errors = []
    polygon_area_rows = []
    content = {name: Counter() for name in archives}
    content_excluded = Counter()
    content_coverage_errors = Counter()
    content_absent_tiles = Counter()
    with ExitStack() as stack:
        opened = {
            name: stack.enter_context(helper.comparison.open_archive(path))
            for name, path in archives.items()
        }
        for xyz in coordinates:
            tiles = {}
            validity = {}
            tile_errors_by_side = {}
            for name, archive in opened.items():
                tile_errors = []
                tiles[name] = helper.decode_tile(
                    archive, xyz, name, Counter(), tile_errors
                )
                tile_errors_by_side[name] = tile_errors
                validity[name] = {
                    (e["layer"], e["id"], e["type"])
                    for e in tile_errors
                    if e["scope"] == "geometry_validity"
                }
                errors.extend(tile_errors)
                for key, features in tiles[name].items():
                    for feature in features:
                        if (
                            feature["geometry"] is None
                            or feature["attrs"] is None
                            or key in validity[name]
                        ):
                            content_excluded[name, key[0]] += 1
                            continue
                        content[name][
                            key[0],
                            xyz,
                            signature([feature], "geometry"),
                            signature([feature], "attrs"),
                        ] += 1
            observed_layers = {key[0] for tile in tiles.values() for key in tile}
            for name, tile_errors in tile_errors_by_side.items():
                for error in tile_errors:
                    if error["scope"] == "absent_tile":
                        for layer in observed_layers | set(helper.LAYERS):
                            content_absent_tiles[name, layer] += 1
                        continue
                    if error["scope"] not in ("tile", "layer"):
                        continue
                    affected = (
                        {error["layer"]}
                        if error.get("layer")
                        else observed_layers | set(helper.LAYERS)
                    )
                    for layer in affected:
                        content_coverage_errors[name, layer] += 1
            keys = sorted(
                set().union(*(tile.keys() for tile in tiles.values())),
                key=lambda k: (k[0], -1 if k[1] is None else k[1], k[2]),
            )
            for key in keys:
                layer, fid, kind = key
                rows = {name: tiles[name].get(key, []) for name in archives}
                counts = {name: len(items) for name, items in rows.items()}
                category = membership_category(
                    counts["baseline"], counts["candidate"], counts["reference"]
                )
                if category:
                    totals["membership", layer, category] += 1
                statuses = {}
                for aspect in ("geometry", "attrs"):
                    before = status(rows["baseline"], rows["reference"], fid, aspect)
                    after = status(rows["candidate"], rows["reference"], fid, aspect)
                    statuses[aspect] = (before, after)
                    totals[aspect, layer, before, after] += 1
                validity_status = {
                    name: key in flags for name, flags in validity.items()
                }
                native_signatures = {
                    aspect: (
                        signature(rows["baseline"], aspect),
                        signature(rows["candidate"], aspect),
                    )
                    for aspect in ("geometry", "attrs")
                }
                native_changes = {
                    aspect: pair[0] != pair[1]
                    for aspect, pair in native_signatures.items()
                }
                if (
                    polygon_area
                    and kind == 3
                    and statuses["geometry"] == ("different", "different")
                    and native_changes["geometry"]
                ):
                    polygon_area_rows.append(
                        {
                            "coordinate": xyz,
                            "layer": layer,
                            "id": fid,
                            "type": kind,
                            **polygon_area_change(
                                rows, validity_status, max_polygon_coordinates
                            ),
                        }
                    )
                for aspect, changed in native_changes.items():
                    if changed and counts["baseline"] == counts["candidate"] == 1:
                        totals["native_value_changed", layer, aspect] += 1
                changed = (
                    category is not None
                    or any(before != after for before, after in statuses.values())
                    or any(native_changes.values())
                    or validity_status["baseline"] != validity_status["candidate"]
                )
                if changed:
                    changes.append(
                        {
                            "coordinate": xyz,
                            "sample_subset": (
                                "original" if xyz in original else "additional"
                            )
                            if original_source
                            else "selected",
                            "layer": layer,
                            "id": fid,
                            "type": kind,
                            "raw_counts": counts,
                            "membership_category": category,
                            "geometry_status": {
                                "before": statuses["geometry"][0],
                                "after": statuses["geometry"][1],
                            },
                            "attrs_status": {
                                "before": statuses["attrs"][0],
                                "after": statuses["attrs"][1],
                            },
                            "native_value_changed": native_changes,
                            "native_signatures": native_signatures,
                            "validity": validity_status,
                        }
                    )
    hashes = {name: helper.comparison.digest(path) for name, path in archives.items()}
    if repeat_archive:
        hashes["candidate_repeat"] = helper.comparison.digest(repeat_archive)
    content_rows = []
    for layer in sorted(
        {key[0] for side in content.values() for key in side}
        | {key[1] for key in content_excluded}
        | {key[1] for key in content_coverage_errors}
        | {key[1] for key in content_absent_tiles}
    ):
        reference_content = content["reference"]
        for side in ("baseline", "candidate"):
            side_content = content[side]
            keys = {
                key
                for key in side_content.keys() | reference_content.keys()
                if key[0] == layer
            }
            matched = sum(
                min(side_content[key], reference_content[key]) for key in keys
            )
            side_total = sum(side_content[key] for key in keys)
            reference_total = sum(reference_content[key] for key in keys)
            excluded = (
                content_excluded[side, layer] + content_excluded["reference", layer]
            )
            coverage_errors = (
                content_coverage_errors[side, layer]
                + content_coverage_errors["reference", layer]
            )
            absent_tiles = (
                content_absent_tiles[side, layer]
                + content_absent_tiles["reference", layer]
            )
            complete = excluded == 0 and coverage_errors == 0 and absent_tiles == 0
            content_rows.append(
                {
                    "layer": layer,
                    "side": side,
                    "matched_without_id": matched,
                    "side_only": side_total - matched,
                    "reference_only": reference_total - matched,
                    "excluded_decode_or_invalid": excluded,
                    "tile_or_layer_decode_errors": coverage_errors,
                    "absent_tile_inputs": absent_tiles,
                    "decoded_content_equal": (
                        side_total == reference_total == matched if complete else None
                    ),
                    "complete_decoded_coverage": complete,
                }
            )
    report = {
        "format_version": 1,
        "scope": "Fixed selected coordinates. Unique tile/layer/id/type keys. Geometry direction-sensitive; typed attrs exact. Absent means both side and reference lack a key. Raw membership, validity and decode failures separate. ID-agnostic content multiset is tile-local and excludes undecoded or invalid features; a missing tile makes decoded-content equality inconclusive. Content equality does not establish ID parity. No whole-archive parity claim.",
        "coordinate_source": {
            "path": str(coordinate_source),
            "sha256": helper.comparison.digest(coordinate_source),
            "selected": len(coordinates),
            "original_source": str(original_source) if original_source else None,
            "original_overlap": len(original) if original_source else None,
            "additional": len(coordinates) - len(original) if original_source else None,
        },
        "archive_paths": {name: str(path) for name, path in archives.items()},
        "archive_sha256": hashes,
        "candidate_repeats_identical": (
            hashes["candidate"] == hashes["candidate_repeat"]
            if repeat_archive
            else None
        ),
        "counts": [
            {
                "dimension": dimension,
                "layer": layer,
                "before_or_category": before,
                "after": after,
                "count": count,
            }
            for (dimension, layer, before, after), count in sorted(
                (key if len(key) == 4 else (*key, None), value)
                for key, value in totals.items()
            )
        ],
        "changed_keys": changes,
        "errors": errors,
        "id_agnostic_content": content_rows,
    }
    if polygon_area:
        measured = [
            row
            for row in polygon_area_rows
            if row["status"] in ("closer", "worse", "same")
        ]
        report["polygon_area_audit"] = {
            "scope": "Only unique polygon keys whose native geometry changed and remains different from reference on selected tiles. Symmetric-difference area in MVT square units, not a visual-quality or whole-map parity claim. Invalid, undecodable, extent-mismatched and over-cap shapes are excluded.",
            "archive_sha256": hashes.copy(),
            "coordinate_source_sha256": report["coordinate_source"]["sha256"],
            "max_combined_coordinates": max_polygon_coordinates,
            "same_area_tolerance_mvt2": 1e-8,
            "counts": dict(Counter(row["status"] for row in polygon_area_rows)),
            "before_area_total_mvt2": sum(row["before_area_mvt2"] for row in measured),
            "after_area_total_mvt2": sum(row["after_area_mvt2"] for row in measured),
            "rows": polygon_area_rows,
        }
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("reference", type=Path)
    parser.add_argument("--coordinates-from", type=Path, required=True)
    parser.add_argument("--original-coordinates-from", type=Path)
    parser.add_argument("--repeat-archive", type=Path)
    parser.add_argument("--polygon-area", action="store_true")
    parser.add_argument(
        "--polygon-area-max-coordinates",
        type=int,
        default=2000,
        help="Maximum total vertices for a three-way polygon overlay; 0 disables the cap",
    )
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    inputs = {args.baseline, args.candidate, args.reference, args.coordinates_from}
    inputs.update(
        path for path in (args.original_coordinates_from, args.repeat_archive) if path
    )
    if args.output.resolve() in {path.resolve() for path in inputs}:
        raise ValueError("Output cannot overwrite input")
    report = audit(
        args.baseline,
        args.candidate,
        args.reference,
        args.coordinates_from,
        args.original_coordinates_from,
        args.repeat_archive,
        args.polygon_area,
        args.polygon_area_max_coordinates,
    )
    args.output.write_text(
        json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    print(args.output)
    print("candidate repeats identical", report["candidate_repeats_identical"])
    for dimension in ("geometry", "attrs"):
        resolved = sum(
            row["count"]
            for row in report["counts"]
            if row["dimension"] == dimension
            and row["before_or_category"] == "different"
            and row["after"] == "equal"
        )
        new = sum(
            row["count"]
            for row in report["counts"]
            if row["dimension"] == dimension
            and row["before_or_category"] == "equal"
            and row["after"] != "equal"
        )
        print(dimension, "resolved", resolved, "equal_to_non_equal", new)
    membership = Counter()
    for row in report["counts"]:
        if row["dimension"] == "membership":
            membership[row["before_or_category"]] += row["count"]
    print("membership", dict(membership))
    print(
        "actual decode errors",
        sum(
            e["scope"] not in ("geometry_validity", "absent_tile")
            for e in report["errors"]
        ),
    )


if __name__ == "__main__":
    main()
