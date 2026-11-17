"""Diagnose OMT label differences on an existing comparison's exact coordinates.

Raw feature counts come from protobuf arrays before geometry decoding. Invalid
features are reported individually and do not truncate later features/layers.
This read-only diagnostic does not establish profile or geometry parity.
"""
import argparse
from collections import Counter, defaultdict
from contextlib import ExitStack
import importlib.util
import json
import math
from pathlib import Path

spec = importlib.util.spec_from_file_location('omt_comparison', Path(__file__).with_name('compare.py'))
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)

LABELS = {'place', 'poi', 'mountain_peak', 'park', 'housenumber', 'aerodrome_label'}


def coordinates_from(path):
    report = json.loads(Path(path).read_text(encoding='utf-8-sig'))
    coordinates = report.get('sampling', {}).get('coordinates', report.get('coordinates'))
    if not isinstance(coordinates, list) or not coordinates:
        raise ValueError('Coordinate source must contain a nonempty sampling.coordinates or coordinates array')
    result = []
    for coordinate in coordinates:
        if not isinstance(coordinate, list) or len(coordinate) != 3 or any(type(v) is not int for v in coordinate):
            raise ValueError('Coordinates must be integer [z, x, y] arrays')
        z, x, y = coordinate
        if not (0 <= z <= 31 and 0 <= x < 2**z and 0 <= y < 2**z):
            raise ValueError('Invalid tile coordinate: ' + str(coordinate))
        result.append(tuple(coordinate))
    if len(set(result)) != len(result):
        raise ValueError('Duplicate selected coordinates would bias occurrence counts')
    return result


def rows(counter, keys):
    return [dict(zip(keys, key), count=count) for key, count in sorted(counter.items())]


def diagnose(left, right, coordinates, example_limit=3):
    raw, types, differences, shared, missing = (Counter() for _ in range(5))
    examples = defaultdict(list)
    errors, collisions, absent_tiles = [], [], []
    with ExitStack() as stack:
        archives = [stack.enter_context(comparison.open_archive(path)) for path in (left, right)]
        for xyz in coordinates:
            decoded = []
            for side, archive in zip(('native', 'planetiler'), archives):
                lookup = defaultdict(list)
                decoded.append(lookup)
                if xyz not in archive.tiles:
                    absent_tiles.append({'side': side, 'coordinate': xyz})
                    continue
                try:
                    decoder = comparison.validator.TileData(archive.tile(xyz), default_options={'y_coord_down': True})
                except Exception as exc:
                    errors.append({'side': side, 'coordinate': xyz, 'scope': 'tile', 'error': str(exc)})
                    continue
                for layer in decoder.tile.layers:
                    raw[side, xyz[0], layer.name] += len(layer.features)
                    for feature in layer.features:
                        types[side, xyz[0], layer.name, feature.type] += 1
                        if layer.name not in LABELS:
                            continue
                        fid = feature.id if feature.HasField('id') else None
                        context = {'side': side, 'coordinate': xyz, 'layer': layer.name, 'id': fid}
                        attrs, typed, geometry, canonical = {}, {}, None, None
                        try:
                            if len(feature.tags) % 2:
                                raise ValueError('Odd attribute tag count')
                            for ki, vi in zip(feature.tags[::2], feature.tags[1::2]):
                                name, value = layer.keys[ki], layer.values[vi]
                                fields = value.ListFields()
                                if name in attrs or len(fields) != 1:
                                    raise ValueError('Duplicate attribute or ambiguous value type')
                                attrs[name] = decoder.parse_value(value)
                                if isinstance(attrs[name], float) and not math.isfinite(attrs[name]):
                                    raise ValueError('Nonfinite attribute')
                                typed[name] = fields[0][0].name
                        except Exception as exc:
                            errors.append(dict(context, scope='attributes', error=str(exc)))
                            attrs = typed = None
                        try:
                            geometry = decoder.parse_geometry(feature.geometry, feature.type, layer.extent, True, None)
                            # Normalization ignores polygon ring starts and multipart
                            # order. This is diagnostic, not a replacement validator.
                            shape = comparison.validator.shape(geometry)
                            if shape.is_empty or not all(math.isfinite(v) for v in shape.bounds):
                                raise ValueError('Empty or nonfinite decoded geometry')
                            canonical = shape.normalize().wkb_hex
                        except Exception as exc:
                            errors.append(dict(context, scope='geometry', error=str(exc)))
                        lookup[layer.name, fid, feature.type].append((attrs, typed, geometry, canonical))
            for key in sorted(decoded[0].keys() | decoded[1].keys(), key=lambda k: (k[0], -1 if k[1] is None else k[1], k[2])):
                layer, fid, kind = key
                ls, rs = decoded[0].get(key, []), decoded[1].get(key, [])
                if len(ls) > 1 or len(rs) > 1:
                    collisions.append({'coordinate': xyz, 'layer': layer, 'id': fid, 'type': kind,
                                       'native_count': len(ls), 'reference_count': len(rs)})
                if not ls or not rs:
                    missing['native_only' if ls else 'reference_only', layer, xyz[0]] += max(len(ls), len(rs))
                    continue
                if len(ls) != 1 or len(rs) != 1:
                    continue
                shared[layer, xyz[0]] += 1
                a, b = ls[0], rs[0]
                if a[0] is not None and b[0] is not None:
                    for name in sorted(a[0].keys() | b[0].keys()):
                        if a[0].get(name) != b[0].get(name) or a[1].get(name) != b[1].get(name):
                            field = layer, xyz[0], name
                            differences[field] += 1
                            if len(examples[field]) < example_limit:
                                examples[field].append({'coordinate': xyz, 'id': fid, 'name': a[0].get('name'),
                                                        'native': a[0].get(name), 'reference': b[0].get(name),
                                                        'native_wire_type': a[1].get(name), 'reference_wire_type': b[1].get(name)})
                if a[3] is not None and b[3] is not None and a[3] != b[3]:
                    field = layer, xyz[0], 'geometry'
                    differences[field] += 1
                    if len(examples[field]) < example_limit:
                        examples[field].append({'coordinate': xyz, 'id': fid, 'name': (a[0] or {}).get('name'),
                                                'native': a[2], 'reference': b[2]})
    return {'format_version': 1, 'status': 'diagnostic',
            'scope': 'Exact externally selected coordinates; raw protobuf counts precede geometry decode. No ring grammar validation or parity claim. Label attributes retain wire types; geometry is normalized for diagnostic matching. Duplicate layer/id/type keys are excluded from pairing and reported.',
            'coordinate_count': len(coordinates), 'coordinates': coordinates,
            'raw_counts_complete_for_selected_tiles': not any(error['scope'] == 'tile' for error in errors),
            'archive_sha256': {'native': comparison.digest(left), 'planetiler': comparison.digest(right)},
            'raw_totals': {side: sum(count for key, count in raw.items() if key[0] == side) for side in ('native', 'planetiler')},
            'raw_layer_zoom_counts': rows(raw, ['side', 'zoom', 'layer']),
            'raw_geometry_type_counts': rows(types, ['side', 'zoom', 'layer', 'mvt_geometry_type']),
            'shared_unique_label_keys': rows(shared, ['layer', 'zoom']),
            'missing_label_keys': rows(missing, ['side', 'layer', 'zoom']),
            'differences': rows(differences, ['layer', 'zoom', 'field']),
            'examples': [{'layer': k[0], 'zoom': k[1], 'field': k[2], 'examples': v} for k, v in sorted(examples.items())],
            'decode_errors': errors, 'duplicate_pairing_keys': collisions, 'absent_selected_tiles': absent_tiles}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('left', type=Path)
    parser.add_argument('right', type=Path)
    parser.add_argument('--comparison', type=Path, help='Comparison JSON supplying sample coordinates')
    parser.add_argument('--coordinates-from', type=Path, help='Override coordinates with baseline comparison/diagnostic JSON')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    try:
        source = args.coordinates_from or args.comparison
        if source is None:
            raise ValueError('Provide --comparison or --coordinates-from')
        if args.output.resolve() in {args.left.resolve(), args.right.resolve(), source.resolve()}:
            raise ValueError('Diagnostic output must not overwrite an archive or coordinate source')
        report = diagnose(args.left, args.right, coordinates_from(source))
        report['coordinate_source'] = {'path': str(source.resolve()), 'sha256': comparison.digest(source)}
        report['comparison_path'] = str(args.comparison.resolve()) if args.comparison else None
        code = 0
    except Exception as exc:
        report, code = {'format_version': 1, 'status': 'error', 'error': str(exc)}, 2
    # Never overwrite a protected input, including when the guard itself failed.
    protected = {args.left.resolve(), args.right.resolve()}
    protected.update(p.resolve() for p in (args.comparison, args.coordinates_from) if p)
    if args.output.resolve() in protected:
        print(json.dumps(report))
        return 2
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=True, allow_nan=False) + '\n', encoding='utf-8')
    return code


if __name__ == '__main__':
    raise SystemExit(main())
