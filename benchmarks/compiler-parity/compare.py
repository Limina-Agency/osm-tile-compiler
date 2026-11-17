"""Compare decoded PMTiles at one deterministic union-coordinate sample.

Exit 0: comparable, valid, equal sampled semantics; 1: unequal/inconclusive;
2: malformed archive, decode failure, or invalid geometry. Sampling never proves
whole-archive parity. IDs and typed attributes are part of this strict comparison.
"""
import argparse
import atexit
from collections import Counter, defaultdict
from contextlib import contextmanager, ExitStack
import hashlib
import importlib.util
import json
import math
import multiprocessing
import re
from pathlib import Path
import sys

VALIDATOR = Path(__file__).resolve().parent / 'validate-pmtiles.py'
spec = importlib.util.spec_from_file_location('pmtiles_validator', VALIDATOR)
validator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validator)


def digest(path):
    h = hashlib.sha256()
    with open(path, 'rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


@contextmanager
def open_archive(path):
    # Archive's constructor can fail after opening/mapping the file. Own its
    # partial state as well so malformed inputs never leave Windows file locks.
    archive = validator.Archive.__new__(validator.Archive)
    try:
        archive.__init__(path)
        if not archive.tiles:
            raise ValueError('Empty archive cannot establish benchmark parity')
        metadata = archive.metadata
        if not isinstance(metadata, dict):
            raise ValueError('Archive metadata must be an object')
        layers = metadata.get('vector_layers', [])
        if not isinstance(layers, list):
            raise ValueError('vector_layers must be an array')
        names = set()
        for layer in layers:
            if not isinstance(layer, dict) or not isinstance(layer.get('id'), str) or not layer['id'] or layer['id'] in names:
                raise ValueError('Invalid or duplicate vector_layers id')
            if not isinstance(layer.get('fields', {}), dict):
                raise ValueError('vector_layers fields must be an object')
            names.add(layer['id'])
        yield archive
    finally:
        for name in ('data', 'file'):
            resource = getattr(archive, name, None)
            if resource is not None:
                resource.close()


def select_coordinates(left, right, count, all_tiles=False):
    by_zoom = defaultdict(list)
    for xyz in set(left) | set(right):
        by_zoom[xyz[0]].append(xyz)
    return sorted(t for tiles in by_zoom.values() for t in
                  (sorted(tiles) if all_tiles else sorted(tiles, key=lambda t:
                   hashlib.sha256(json.dumps(t).encode()).digest())[:count]))


def gate(left, right, lm, rm, identical, hashes):
    reasons = []
    if not identical or lm or rm:
        for key in ('input_sha256', 'schema', 'profile', 'settings'):
            if key not in lm or key not in rm:
                reasons.append('missing_manifest_' + key)
            elif lm[key] != rm[key]:
                reasons.append('different_' + key)
        for key in ('minzoom', 'maxzoom', 'detailzoom', 'output', 'annotations', 'auxiliary_inputs', 'bounds_policy', 'tile_compression'):
            if any(not isinstance(m.get('settings'), dict) or key not in m['settings'] for m in (lm, rm)):
                reasons.append('missing_settings_' + key)
        for side, manifest, archive, archive_hash in (('left', lm, left, hashes[0]), ('right', rm, right, hashes[1])):
            for key in ('schema', 'profile'):
                if not isinstance(manifest.get(key), str) or not manifest[key].strip():
                    reasons.append(side + '_invalid_' + key)
            if manifest.get('artifact_sha256') != archive_hash:
                reasons.append(side + '_missing_or_mismatched_artifact_sha256')
            if not isinstance(manifest.get('input_sha256'), str) or not re.fullmatch('[0-9a-fA-F]{64}', manifest['input_sha256']):
                reasons.append(side + '_invalid_input_sha256')
            settings = manifest.get('settings', {})
            if isinstance(settings, dict):
                for key in ('bounds_policy', 'output', 'annotations'):
                    if not isinstance(settings.get(key), str) or not settings[key].strip():
                        reasons.append(side + '_invalid_' + key)
            if isinstance(settings, dict) and settings.get('tile_compression') != archive.header['tile_compression'].name.lower():
                reasons.append(side + '_mismatched_tile_compression')
            auxiliary = settings.get('auxiliary_inputs') if isinstance(settings, dict) else None
            if not isinstance(auxiliary, dict) or any(not isinstance(v, str) or not re.fullmatch('[0-9a-fA-F]{64}', v) for v in auxiliary.values()):
                reasons.append(side + '_invalid_auxiliary_hashes')
    for key in ('min_zoom', 'max_zoom', 'min_lon_e7', 'min_lat_e7', 'max_lon_e7', 'max_lat_e7'):
        if left.header.get(key) != right.header.get(key):
            reasons.append('different_header_' + key)
    # Descriptions and producer names are not schema, but fields/zoom ranges are.
    def schema(a):
        return sorted(({k: layer[k] for k in ('id', 'fields', 'minzoom', 'maxzoom') if k in layer}
                       for layer in a.metadata.get('vector_layers', [])), key=lambda x: x['id'])
    if not schema(left) or not schema(right):
        reasons.append('missing_vector_layers_metadata')
    elif schema(left) != schema(right):
        reasons.append('different_metadata_schema')
    return {'comparable': not reasons, 'reasons': reasons,
            'mode': 'same_archive_identity_control' if identical else 'cross_compiler',
            'native_omt_support': 'native experimental profile available; parity not asserted'}


def decoded(archive, xyz, inventory, validity=None):
    validity = validity if validity is not None else []
    decoder = validator.TileData(archive.tile(xyz), default_options={'y_coord_down': True})
    result = {}
    declared = {l['id'] for l in archive.metadata.get('vector_layers', [])}
    for layer in decoder.tile.layers:
        if layer.name in result or layer.name not in declared or layer.extent <= 0 or layer.version not in (1, 2):
            raise ValueError('Duplicate/undeclared layer or invalid extent/version')
        features = []
        for feature in layer.features:
            validator.commands(feature)
            if len(feature.tags) % 2:
                raise ValueError('Odd tag count')
            attrs = {}
            for ki, vi in zip(feature.tags[::2], feature.tags[1::2]):
                if ki >= len(layer.keys) or vi >= len(layer.values):
                    raise ValueError('Invalid tag reference')
                key, value = layer.keys[ki], layer.values[vi]
                fields = value.ListFields()
                if key in attrs or len(fields) != 1:
                    raise ValueError('Duplicate attribute or ambiguous value type')
                field, raw = fields[0]
                attrs[key] = [field.name, raw]
                inventory[f'{xyz[0]}/{layer.name}/attribute/{key}/{field.name}'] += 1
            geometry = validator.shape(decoder.parse_geometry(feature.geometry, feature.type, layer.extent, True, None))
            if geometry.is_empty or not all(math.isfinite(v) for v in geometry.bounds) or not geometry.is_valid:
                validity.append({'tile': xyz, 'layer': layer.name, 'feature_id': feature.id,
                                 'error': 'Empty/nonfinite geometry' if geometry.is_empty or not all(math.isfinite(v) for v in geometry.bounds)
                                 else validator.explain_validity(geometry)})
            # Polygon ring starts and orientation are geometric representation.
            # Line direction can affect oneway arrows/offsets, so preserve it.
            canonical_geometry = (sorted(g.wkb_hex for g in geometry.geoms) if geometry.geom_type == 'MultiLineString'
                                  else geometry.wkb_hex if geometry.geom_type == 'LineString'
                                  else geometry.normalize().wkb_hex)
            features.append(json.dumps([feature.id if feature.HasField('id') else None,
                                        attrs, canonical_geometry], sort_keys=True, allow_nan=False))
            inventory[f'{xyz[0]}/{layer.name}/features'] += 1
            inventory[f'{xyz[0]}/{layer.name}/geometry/{geometry.geom_type}'] += 1
        inventory[f'{xyz[0]}/{layer.name}/tiles'] += 1
        result[layer.name] = {'extent': layer.extent, 'features': sorted(features)}
    return result


def _compare_tile(archives, xyz):
    inventories = [Counter(), Counter()]
    values, errors, validity = [], [], []
    for i, archive in enumerate(archives):
        feature_validity = []
        try:
            values.append(decoded(archive, xyz, inventories[i], feature_validity) if xyz in archive.tiles else None)
        except Exception as exc:
            errors.append({'tile': xyz, 'side': ('left', 'right')[i], 'error': str(exc)})
            values.append({'decode_error': str(exc)})
        finally:
            validity.extend(dict(issue, side=('left', 'right')[i]) for issue in feature_validity)
    difference = ({'tile': xyz, 'kind': 'missing_tile' if None in values else 'decoded_semantics'}
                  if values[0] != values[1] else None)
    # Transfer only the report contribution; decoded feature geometry remains in
    # the worker and archive handles are reused for its next coordinate.
    return inventories, difference, errors, validity


_worker_stack = None
_worker_archives = None


def _open_worker_archives(left_path, right_path):
    global _worker_stack, _worker_archives
    _worker_stack = ExitStack()
    atexit.register(_worker_stack.close)
    _worker_archives = (_worker_stack.enter_context(open_archive(left_path)),
                        _worker_stack.enter_context(open_archive(right_path)))


def _worker_compare_tile(xyz):
    return _compare_tile(_worker_archives, xyz)


def compare(left_path, right_path, lm=None, rm=None, count=32, all_tiles=False, workers=1):
    if count < 1:
        raise ValueError('sample-per-zoom must be positive')
    if workers < 1:
        raise ValueError('workers must be positive')
    with ExitStack() as stack:
        left = stack.enter_context(open_archive(left_path))
        right = stack.enter_context(open_archive(right_path))
        if any(m is not None and not isinstance(m, dict) for m in (lm, rm)):
            raise ValueError('Comparison manifests must be JSON objects')
        hashes = (digest(left_path), digest(right_path))
        identical = hashes[0] == hashes[1]
        comparison_gate = gate(left, right, lm or {}, rm or {}, identical, hashes)
        selected = select_coordinates(left.tiles, right.tiles, count, all_tiles)
        inventories = [Counter(), Counter()]
        differences, errors, validity = [], [], []
        if workers == 1:
            outcomes = (_compare_tile((left, right), xyz) for xyz in selected)
        else:
            pool = stack.enter_context(multiprocessing.get_context('spawn').Pool(
                processes=workers, initializer=_open_worker_archives,
                initargs=(str(left_path), str(right_path))))
            outcomes = pool.imap(_worker_compare_tile, selected, chunksize=16)
        for tile_inventory, difference, tile_errors, tile_validity in outcomes:
            for total, contribution in zip(inventories, tile_inventory):
                total.update(contribution)
            if difference is not None:
                differences.append(difference)
            errors.extend(tile_errors)
            validity.extend(tile_validity)
        coverage = {}
        for z in sorted({t[0] for t in set(left.tiles) | set(right.tiles)}):
            l, r = ({t for t in a.tiles if t[0] == z} for a in (left, right))
            coverage[z] = dict(left=len(l), right=len(r), shared=len(l & r), left_only=len(l-r), right_only=len(r-l))
        coverage_equal = set(left.tiles) == set(right.tiles)
        equal = coverage_equal and not differences and not errors
        status = ('invalid' if errors or validity else 'unequal_work_or_unverified_inputs' if not comparison_gate['comparable']
                  else 'different' if not equal else 'equal_full_decode' if all_tiles else 'equal_sample')
        return {'format_version': 1, 'status': status, 'gate': comparison_gate,
                'manifests': {'left': lm, 'right': rm},
                'metadata': {'left': left.metadata, 'right': right.metadata},
                'archive_sha256': dict(zip(('left', 'right'), hashes)),
                'archive_headers': {side: {k: v.name.lower() if hasattr(v, 'name') else v for k, v in archive.header.items()}
                                    for side, archive in (('left', left), ('right', right))},
                'byte_identical': identical, 'semantic_equal_on_sample': equal,
                'sample_geometry_valid': not validity and not errors,
                'whole_archive_semantic_parity': bool(all_tiles and equal and not validity and comparison_gate['comparable']),
                'sampling': {'method': 'sha256-ranked union coordinates per zoom', 'all_tiles': all_tiles,
                             'sample_per_zoom': count, 'coordinates': selected},
                'coverage': coverage, 'sample_inventory': {'left': dict(inventories[0]), 'right': dict(inventories[1])},
                'differences': differences, 'structural_errors': errors, 'geometry_validity_issues': validity,
                'errors': errors}, (2 if errors or validity else 0 if equal and comparison_gate['comparable'] else 1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('left')
    parser.add_argument('right')
    parser.add_argument('--left-manifest')
    parser.add_argument('--right-manifest')
    parser.add_argument('--output', required=True)
    parser.add_argument('--sample-per-zoom', type=int, default=32)
    parser.add_argument('--all-tiles', action='store_true')
    parser.add_argument('--workers', type=int, default=1, help='bounded tile decode processes (default: 1)')
    args = parser.parse_args()
    try:
        manifests = [json.loads(Path(p).read_text(encoding='utf-8-sig')) if p else {} for p in (args.left_manifest, args.right_manifest)]
        report, code = compare(args.left, args.right, *manifests, args.sample_per_zoom, args.all_tiles, args.workers)
    except Exception as exc:
        report, code = {'format_version': 1, 'status': 'error', 'error': str(exc)}, 2
    Path(args.output).parent.mkdir(parents=True, exist_ok=True)
    Path(args.output).write_text(json.dumps(report, indent=2, allow_nan=False) + '\n', encoding='utf-8')
    return code


if __name__ == '__main__':
    sys.exit(main())
