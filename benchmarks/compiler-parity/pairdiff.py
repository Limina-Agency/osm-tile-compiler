"""Measure every differing feature pair of a whole-archive comparison against the parity bar.

Reads the tiles a `compare.py --all-tiles` report lists as different, decodes
every feature on both sides and pairs them by layer, ID and geometry type. Each
pair is checked against the bar in docs/parity.md:

1. features pair by ID, with the same typed attributes;
2. lines and points are identical;
3. polygons are identical, or differ by less than 1% of the reference area
   (symmetric difference) and less than 1 pixel of a 256-pixel tile
   (vertex-based Hausdorff distance, as GEOS's discrete Hausdorff);
4. no polygon is invalid in native where the reference's is valid.

Nothing is sampled and no pair is skipped for its size: large polygons use an
exact spatial-index form of the same Hausdorff distance. Tiles that compare.py
found equal hold identical features, so they cannot fail points 1 to 3; point 4
is checked over every tile from compare.py's validity list.

Exit 0: every pair meets the bar (accepted cases excepted); 1: some do not;
2: the inputs do not match or could not be decoded.
"""

import argparse
from collections import Counter, defaultdict
from contextlib import ExitStack
import atexit
import importlib.util
import json
import math
import multiprocessing
from pathlib import Path
import sys

import numpy as np
import shapely
from shapely.geometry import shape

spec = importlib.util.spec_from_file_location("omt_compare", Path(__file__).with_name("compare.py"))
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)

AREA_BAR = 0.01
DISTANCE_BAR_PX = 1.0
TYPES = {1: "point", 2: "line", 3: "polygon"}
# Above this many vertices on both sides together, Hausdorff distance uses the
# indexed form instead of GEOS's quadratic one. Both give the same value.
INDEXED_HAUSDORFF_FROM = 4000


def tile_features(archive, xyz):
    """Decoded features of one tile, keyed by (layer, id, type)."""
    if xyz not in archive.tiles:
        return None
    decoder = comparison.validator.TileData(archive.tile(xyz), default_options={"y_coord_down": True})
    rows = defaultdict(list)
    for layer in decoder.tile.layers:
        for feature in layer.features:
            attrs = {}
            for ki, vi in zip(feature.tags[::2], feature.tags[1::2]):
                field, raw = layer.values[vi].ListFields()[0]
                attrs[layer.keys[ki]] = [field.name, raw]
            geometry = shape(decoder.parse_geometry(feature.geometry, feature.type, layer.extent, True, None))
            key = (layer.name, feature.id if feature.HasField("id") else None, TYPES.get(feature.type, str(feature.type)))
            rows[key].append({"extent": layer.extent, "attrs": attrs, "geometry": geometry,
                              "canonical": canonical(geometry)})
    return rows


def canonical(geometry):
    # As compare.py: polygon ring starts and orientation are representation,
    # line direction is not.
    if geometry.geom_type == "MultiLineString":
        return tuple(sorted(g.wkb_hex for g in geometry.geoms))
    if geometry.geom_type == "LineString":
        return geometry.wkb_hex
    return geometry.normalize().wkb_hex


def pair_rows(left, right):
    """Pairs two lists of features under one key: identical ones first, then by order."""
    left, right = list(left), list(right)
    pairs = []
    for l in list(left):
        match = next((r for r in right if r["canonical"] == l["canonical"] and r["attrs"] == l["attrs"]), None)
        if match is not None:
            pairs.append((l, match))
            left.remove(l)
            right.remove(match)
    pairs.extend(zip(left, right))
    n = min(len(left), len(right))
    return pairs, left[n:], right[n:]


def rings(geometry):
    for polygon in getattr(geometry, "geoms", [geometry]):
        if polygon.is_empty:
            continue
        yield np.asarray(polygon.exterior.coords)
        for interior in polygon.interiors:
            yield np.asarray(interior.coords)


def directed_hausdorff(source, target):
    """Largest distance from a vertex of `source` to the rings of `target`."""
    segments = [shapely.linestrings(np.stack([ring[:-1], ring[1:]], axis=1)) for ring in rings(target)]
    vertices = [ring for ring in rings(source)]
    if not segments or not vertices:
        return math.inf
    tree = shapely.STRtree(np.concatenate(segments))
    points = shapely.points(np.concatenate(vertices))
    _, distances = tree.query_nearest(points, return_distance=True, all_matches=False)
    return float(distances.max())


def hausdorff(left, right):
    count = shapely.get_num_coordinates(left) + shapely.get_num_coordinates(right)
    if count < INDEXED_HAUSDORFF_FROM:
        return float(left.hausdorff_distance(right))
    return max(directed_hausdorff(left, right), directed_hausdorff(right, left))


def measure_polygons(native, reference, extent):
    """Area share and Hausdorff distance of two polygon geometries, in 256-pixel units."""
    nvalid, rvalid = bool(native.is_valid), bool(reference.is_valid)
    left = native if nvalid else shapely.make_valid(native)
    right = reference if rvalid else shapely.make_valid(reference)
    difference = left.symmetric_difference(right).area
    scale = 256.0 / extent
    area_share = difference / right.area if right.area else (0.0 if difference == 0 else None)
    distance = hausdorff(native, reference) * scale
    distance = distance if math.isfinite(distance) else None
    within = area_share is not None and distance is not None and area_share < AREA_BAR and distance < DISTANCE_BAR_PX
    return {"native_valid": nvalid, "reference_valid": rvalid,
            "area_share": area_share, "hausdorff_px": distance,
            "reference_area_px2": right.area * scale * scale,
            "native_vertices": int(shapely.get_num_coordinates(native)),
            "reference_vertices": int(shapely.get_num_coordinates(reference)),
            "within_bar": within}


def compare_tile(native, reference, xyz):
    """Every difference in one tile, as rows with a `kind`."""
    left, right = tile_features(native, xyz), tile_features(reference, xyz)
    if left is None or right is None:
        return [{"tile": list(xyz), "kind": "missing_tile", "side": "native" if left is None else "reference"}]
    out = []
    for key in sorted(set(left) | set(right), key=lambda k: (k[0], k[1] is None, k[1] or 0, k[2])):
        layer, fid, kind = key
        base = {"tile": list(xyz), "layer": layer, "id": fid, "type": kind}
        pairs, native_only, reference_only = pair_rows(left.get(key, []), right.get(key, []))
        for side, rows in (("native", native_only), ("reference", reference_only)):
            for row in rows:
                out.append(dict(base, kind="unpaired", side=side, vertices=int(shapely.get_num_coordinates(row["geometry"]))))
        for l, r in pairs:
            if l["attrs"] != r["attrs"]:
                changed = sorted(k for k in set(l["attrs"]) | set(r["attrs"]) if l["attrs"].get(k) != r["attrs"].get(k))
                out.append(dict(base, kind="attributes", changed={k: [l["attrs"].get(k), r["attrs"].get(k)] for k in changed}))
            if l["canonical"] == r["canonical"]:
                continue
            if kind != "polygon":
                out.append(dict(base, kind=kind + "_geometry",
                                native_vertices=int(shapely.get_num_coordinates(l["geometry"])),
                                reference_vertices=int(shapely.get_num_coordinates(r["geometry"]))))
                continue
            try:
                metric = measure_polygons(l["geometry"], r["geometry"], l["extent"])
            except Exception as error:  # an overlay failure is a result, not a pass
                metric = {"error": str(error), "within_bar": False}
            out.append(dict(base, kind="polygon_geometry", **metric))
    return out


def invalid_only_in_native(report):
    """Features compare.py found invalid in native but not in the reference, over every tile."""
    sides = defaultdict(dict)
    for issue in report.get("geometry_validity_issues", []):
        sides[issue["side"]][(tuple(issue["tile"]), issue["layer"], issue["feature_id"])] = issue["error"]
    return [{"tile": list(tile), "layer": layer, "id": fid, "kind": "invalid_only_in_native", "reason": error}
            for (tile, layer, fid), error in sorted(sides["left"].items(), key=lambda item: (item[0][0], item[0][1], item[0][2] or 0))
            if (tile, layer, fid) not in sides["right"]]


_archives = None


def _open(native_path, reference_path):
    global _archives
    stack = ExitStack()
    atexit.register(stack.close)
    _archives = (stack.enter_context(comparison.open_archive(native_path)),
                 stack.enter_context(comparison.open_archive(reference_path)))


def _work(xyz):
    try:
        return compare_tile(*_archives, tuple(xyz))
    except Exception as error:
        return [{"tile": list(xyz), "kind": "decode_error", "error": str(error)}]


def load_accepted(path):
    if not path:
        return []
    return json.loads(Path(path).read_text(encoding="utf-8"))


def is_accepted(row, accepted):
    for rule in accepted:
        if all(row.get(k) == v for k, v in rule.items() if k != "reason"):
            return rule.get("reason", "accepted")
    return None


def failing(row):
    if row["kind"] == "polygon_geometry":
        return not row["within_bar"]
    return True


def summarise(rows, accepted):
    by_layer = defaultdict(Counter)
    failures, accepted_rows = [], []
    for row in rows:
        layer = row.get("layer", "-")
        by_layer[layer][row["kind"]] += 1
        if row["kind"] == "polygon_geometry" and row["within_bar"]:
            by_layer[layer]["polygon_within_bar"] += 1
        if failing(row):
            reason = is_accepted(row, accepted)
            if reason:
                accepted_rows.append(dict(row, accepted=reason))
            else:
                failures.append(row)
    return {layer: dict(counts) for layer, counts in sorted(by_layer.items())}, failures, accepted_rows


def run(native_path, reference_path, report, workers=1, accepted=()):
    hashes = comparison.digest(native_path), comparison.digest(reference_path)
    recorded = report.get("archive_sha256", {})
    if (recorded.get("left"), recorded.get("right")) != hashes:
        raise ValueError("Comparison report is not bound to these native/reference archives")
    if not report.get("sampling", {}).get("all_tiles"):
        raise ValueError("Comparison report must come from compare.py --all-tiles")
    tiles = sorted({tuple(d["tile"]) for d in report["differences"]})
    if workers == 1:
        _open(str(native_path), str(reference_path))
        results = map(_work, tiles)
        rows = [row for result in results for row in result]
    else:
        with multiprocessing.get_context("spawn").Pool(workers, _open, (str(native_path), str(reference_path))) as pool:
            rows = [row for result in pool.imap(_work, tiles, chunksize=1) for row in result]
    rows += invalid_only_in_native(report)
    invalid_only_in_reference = sum(
        1 for row in invalid_only_in_native({"geometry_validity_issues": [
            dict(issue, side={"left": "right", "right": "left"}[issue["side"]])
            for issue in report.get("geometry_validity_issues", [])]}))
    layers, failures, accepted_rows = summarise(rows, accepted)
    return {"format_version": 1,
            "bar": {"area_share_below": AREA_BAR, "hausdorff_px_below": DISTANCE_BAR_PX, "tile_px": 256},
            "archive_sha256": {"native": hashes[0], "reference": hashes[1]},
            "tiles_in_archive": sum(c["shared"] + c["left_only"] + c["right_only"] for c in report["coverage"].values()),
            "differing_tiles": len(tiles),
            "comparison_status": report.get("status"),
            "meets_bar": not failures and report.get("status") != "error",
            "invalid_polygons": {"native": sum(i["side"] == "left" for i in report.get("geometry_validity_issues", [])),
                                 "reference": sum(i["side"] == "right" for i in report.get("geometry_validity_issues", [])),
                                 "only_in_reference": invalid_only_in_reference},
            "by_layer": layers,
            "outside_bar": failures,
            "accepted": accepted_rows,
            "differences": rows}


def markdown(result):
    lines = [f"Differing tiles: {result['differing_tiles']} of {result['tiles_in_archive']}",
             f"Meets the bar: {'yes' if result['meets_bar'] else 'no'}",
             f"Invalid polygons: {result['invalid_polygons']['native']} native, "
             f"{result['invalid_polygons']['reference']} reference, "
             f"{result['invalid_polygons']['only_in_reference']} only in the reference", "",
             "| Layer | Kind | Count |", "|---|---|---|"]
    for layer, counts in result["by_layer"].items():
        for kind, count in sorted(counts.items()):
            lines.append(f"| {layer} | {kind} | {count} |")
    for title, rows in (("Outside the bar", result["outside_bar"]), ("Accepted", result["accepted"])):
        lines += ["", f"{title}: {len(rows)}"]
        for row in rows:
            z, x, y = row.get("tile", ["?"] * 3)
            detail = {k: v for k, v in row.items() if k not in ("tile", "layer", "id", "type", "kind")}
            lines.append(f"- {z}/{x}/{y} {row.get('layer')} {row.get('id')} {row['kind']}: {json.dumps(detail)}")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("native", type=Path)
    parser.add_argument("reference", type=Path)
    parser.add_argument("--comparison-report", type=Path, required=True, help="compare.py --all-tiles output")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--markdown", type=Path, help="also write a short summary")
    parser.add_argument("--accepted", type=Path, help="JSON list of accepted differences, each matched on its fields")
    parser.add_argument("--workers", type=int, default=1)
    args = parser.parse_args()
    for path in (args.native, args.reference, args.comparison_report):
        if args.output.resolve() == path.resolve():
            raise SystemExit("Output path must differ from the inputs")
    try:
        report = json.loads(args.comparison_report.read_text(encoding="utf-8-sig"))
        result = run(args.native, args.reference, report, args.workers, load_accepted(args.accepted))
        code = 0 if result["meets_bar"] else 1
    except Exception as error:
        result, code = {"format_version": 1, "status": "error", "error": str(error)}, 2
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, allow_nan=False, default=str) + "\n", encoding="utf-8")
    if args.markdown and code != 2:
        args.markdown.write_text(markdown(result), encoding="utf-8")
    print(f"differing tiles {result.get('differing_tiles')}, outside the bar {len(result.get('outside_bar', []))}, "
          f"accepted {len(result.get('accepted', []))}" if code != 2 else result["error"])
    return code


if __name__ == "__main__":
    sys.exit(main())
