"""All-layer, fixed-coordinate PMTiles diagnostic; no archive-wide parity claim."""

import argparse
from collections import Counter, defaultdict
from contextlib import ExitStack
import hashlib
import importlib.util
import json
import math
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "omt_diagnose", Path(__file__).with_name("diagnose_omt.py")
)
diag = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diag)
comparison = diag.comparison
FORMAT_VERSION = 1
LAYERS = (
    "water",
    "waterway",
    "landcover",
    "landuse",
    "mountain_peak",
    "park",
    "boundary",
    "aeroway",
    "transportation",
    "building",
    "water_name",
    "transportation_name",
    "place",
    "housenumber",
    "poi",
    "aerodrome_label",
)


def geometry_key(shape):
    # Match compare.py: line direction is meaningful; polygon starts are not.
    if shape.geom_type == "MultiLineString":
        return tuple(sorted(geometry.wkb_hex for geometry in shape.geoms))
    if shape.geom_type == "LineString":
        return shape.wkb_hex
    return shape.normalize().wkb_hex


def decode_tile(archive, xyz, side, raw, errors):
    lookup = defaultdict(list)
    if xyz not in archive.tiles:
        errors.append({"side": side, "coordinate": xyz, "scope": "absent_tile"})
        return lookup
    try:
        decoder = comparison.validator.TileData(
            archive.tile(xyz), default_options={"y_coord_down": True}
        )
    except Exception as exc:
        errors.append(
            {"side": side, "coordinate": xyz, "scope": "tile", "error": str(exc)}
        )
        return lookup
    for layer in decoder.tile.layers:
        raw[side, layer.name, xyz[0]] += len(layer.features)
        for feature in layer.features:
            fid = feature.id if feature.HasField("id") else None
            context = {
                "side": side,
                "coordinate": xyz,
                "layer": layer.name,
                "id": fid,
                "type": feature.type,
            }
            attrs = None
            geometry = None
            try:
                if len(feature.tags) % 2:
                    raise ValueError("Odd attribute tags")
                attrs = {}
                for ki, vi in zip(feature.tags[::2], feature.tags[1::2]):
                    name, value = layer.keys[ki], layer.values[vi]
                    fields = value.ListFields()
                    if name in attrs or len(fields) != 1:
                        raise ValueError("Duplicate/ambiguous attribute")
                    parsed = decoder.parse_value(value)
                    if isinstance(parsed, float) and not math.isfinite(parsed):
                        raise ValueError("Nonfinite attribute")
                    attrs[name] = [fields[0][0].name, parsed]
            except Exception as exc:
                errors.append(dict(context, scope="attributes", error=str(exc)))
            try:
                comparison.validator.commands(feature)
                shape = comparison.validator.shape(
                    decoder.parse_geometry(
                        feature.geometry, feature.type, layer.extent, True, None
                    )
                )
                if shape.is_empty or not all(math.isfinite(v) for v in shape.bounds):
                    raise ValueError("Empty/nonfinite geometry")
                geometry = geometry_key(shape)
                if not shape.is_valid:
                    errors.append(
                        dict(
                            context,
                            scope="geometry_validity",
                            error=comparison.validator.explain_validity(shape),
                        )
                    )
            except Exception as exc:
                errors.append(dict(context, scope="geometry", error=str(exc)))
            lookup[layer.name, fid, feature.type].append(
                {"attrs": attrs, "geometry": geometry, "extent": layer.extent}
            )
    return lookup


def pair(left, right, xyz, counts, examples, limit=3):
    collisions = []
    for key in sorted(
        left.keys() | right.keys(),
        key=lambda k: (k[0], -1 if k[1] is None else k[1], k[2]),
    ):
        layer, fid, kind = key
        native, reference = left.get(key, []), right.get(key, [])
        if fid is None or len(native) > 1 or len(reference) > 1:
            collisions.append(
                {
                    "coordinate": xyz,
                    "layer": layer,
                    "id": fid,
                    "type": kind,
                    "native_count": len(native),
                    "reference_count": len(reference),
                }
            )
            counts[layer, "ambiguous_keys"] += 1
            continue
        if not native or not reference:
            side = "native_only" if native else "reference_only"
            counts[layer, side] += len(native or reference)
            if len(examples[layer, side]) < limit:
                examples[layer, side].append(
                    {
                        "coordinate": xyz,
                        "id": fid,
                        "type": kind,
                        "features": len(native or reference),
                    }
                )
            continue
        counts[layer, "shared_unique_keys"] += 1
        native_feature, reference_feature = native[0], reference[0]
        native_attrs = native_feature["attrs"]
        reference_attrs = reference_feature["attrs"]
        if native_attrs is None or reference_attrs is None:
            counts[layer, "attribute_pair_inconclusive"] += 1
        else:
            for field in sorted(native_attrs.keys() | reference_attrs.keys()):
                native_value = native_attrs.get(field)
                reference_value = reference_attrs.get(field)
                if native_value == reference_value:
                    continue
                category = (
                    "typed_attribute"
                    if native_value is not None
                    and reference_value is not None
                    and native_value[1] == reference_value[1]
                    else "attribute"
                )
                counts[layer, category] += 1
                counts[layer, "attribute_occurrences"] += 1
                if len(examples[layer, category]) < limit:
                    examples[layer, category].append(
                        {
                            "coordinate": xyz,
                            "id": fid,
                            "field": field,
                            "native": native_value,
                            "reference": reference_value,
                        }
                    )

        native_geometry = native_feature["geometry"]
        reference_geometry = reference_feature["geometry"]
        if native_geometry is None or reference_geometry is None:
            counts[layer, "geometry_pair_inconclusive"] += 1
            if len(examples[layer, "geometry_pair_inconclusive"]) < limit:
                examples[layer, "geometry_pair_inconclusive"].append(
                    {
                        "coordinate": xyz,
                        "id": fid,
                        "type": kind,
                        "native_decoded": native_geometry is not None,
                        "reference_decoded": reference_geometry is not None,
                    }
                )
        else:
            if (
                native_feature["extent"] != reference_feature["extent"]
                or native_geometry != reference_geometry
            ):
                counts[layer, "geometry"] += 1
                if len(examples[layer, "geometry"]) < limit:
                    examples[layer, "geometry"].append(
                        {
                            "coordinate": xyz,
                            "id": fid,
                            "type": kind,
                            "native_geometry_sha256": hashlib.sha256(
                                str(native_geometry).encode()
                            ).hexdigest(),
                            "reference_geometry_sha256": hashlib.sha256(
                                str(reference_geometry).encode()
                            ).hexdigest(),
                        }
                    )
    return collisions


def analyze(left, right, coordinates, limit=3):
    raw = Counter()
    counts = Counter()
    examples = defaultdict(list)
    errors = []
    collisions = []
    with ExitStack() as stack:
        a = stack.enter_context(comparison.open_archive(left))
        b = stack.enter_context(comparison.open_archive(right))
        for xyz in coordinates:
            x = decode_tile(a, xyz, "native", raw, errors)
            y = decode_tile(b, xyz, "reference", raw, errors)
            collisions.extend(pair(x, y, xyz, counts, examples, limit))
    layers = []
    for layer in sorted(set(LAYERS) | {k[1] for k in raw}):
        native = sum(
            v
            for (side, name, _), v in raw.items()
            if side == "native" and name == layer
        )
        reference = sum(
            v
            for (side, name, _), v in raw.items()
            if side == "reference" and name == layer
        )
        metrics = {metric: v for (name, metric), v in counts.items() if name == layer}
        layers.append(
            {
                "layer": layer,
                "native_features": native,
                "reference_features": reference,
                "count_delta": native - reference,
                **metrics,
            }
        )
    return {
        "format_version": FORMAT_VERSION,
        "scope": "Fixed coordinate sample only; raw counts precede feature decoding. Pair only unique layer/id/type; ambiguous IDs and decode errors reported separately. Geometry normalizes polygon ring start/orientation; line direction preserved. No whole-archive parity claim.",
        "coordinates": coordinates,
        "archive_sha256": {
            "native": comparison.digest(left),
            "reference": comparison.digest(right),
        },
        "layers": layers,
        "raw_layer_zoom_counts": [
            {"side": side, "layer": layer, "zoom": zoom, "count": v}
            for (side, layer, zoom), v in sorted(raw.items())
        ],
        "examples": [
            {"layer": layer, "category": category, "items": items}
            for (layer, category), items in sorted(examples.items())
        ],
        "ambiguous_keys": collisions,
        "errors": errors,
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("native", type=Path)
    p.add_argument("reference", type=Path)
    p.add_argument("--coordinates-from", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--example-limit", type=int, default=3)
    a = p.parse_args()
    if a.example_limit < 0:
        raise ValueError("example limit must be nonnegative")
    if a.output.resolve() in {
        a.native.resolve(),
        a.reference.resolve(),
        a.coordinates_from.resolve(),
    }:
        raise ValueError("Output cannot overwrite input")
    report = analyze(
        a.native,
        a.reference,
        diag.coordinates_from(a.coordinates_from),
        a.example_limit,
    )
    report["coordinate_source"] = {
        "path": str(a.coordinates_from.resolve()),
        "sha256": comparison.digest(a.coordinates_from),
    }
    a.output.parent.mkdir(parents=True, exist_ok=True)
    a.output.write_text(
        json.dumps(report, indent=2, ensure_ascii=False, allow_nan=False) + "\n", encoding="utf-8"
    )


if __name__ == "__main__":
    main()
