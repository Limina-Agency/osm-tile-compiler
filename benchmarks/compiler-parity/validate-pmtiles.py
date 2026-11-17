"""Validate PMTiles directories completely and deterministic decoded MVT samples.

Schema counts and source probes are diagnostic, never cross-profile equality tests.
Use --all-tiles to decode the entire archive; topology checks use GEOS/Shapely.
"""
import argparse
from collections import Counter, defaultdict
import gzip
import hashlib
import io
import json
import math
import mmap
from pathlib import Path

from pmtiles.tile import deserialize_header, tileid_to_zxy, read_varint, Compression
from mapbox_vector_tile.decoder import TileData
from shapely.geometry import shape, Point
from shapely.validation import explain_validity


def decompress(data, compression):
    if compression == Compression.NONE:
        return data
    if compression == Compression.GZIP:
        return gzip.decompress(data)
    raise ValueError(f'Unsupported compression {compression}')


class Archive:
    def __init__(self, path):
        self.file = open(path, 'rb')
        self.data = mmap.mmap(self.file.fileno(), 0, access=mmap.ACCESS_READ)
        self.header = h = deserialize_header(self.read(0, 127))
        if h['version'] != 3 or h['tile_type'].name != 'MVT':
            raise ValueError('Expected PMTiles v3 MVT')
        for prefix in ('root', 'metadata', 'leaf_directory', 'tile_data'):
            self.read(h[prefix + '_offset'], h[prefix + '_length'])
        self.metadata = json.loads(decompress(self.read(h['metadata_offset'], h['metadata_length']), h['internal_compression']))
        self.tiles = {}
        self.entries = 0
        self.contents = set()
        self.directories = set()
        self.walk(h['root_offset'], h['root_length'], 0)
        if len(self.tiles) != h['addressed_tiles_count']:
            raise ValueError('Addressed tile count differs from header')
        if self.entries != h['tile_entries_count'] or len(self.contents) != h['tile_contents_count']:
            raise ValueError('Directory entry/content count differs from header')

    def read(self, offset, length):
        if offset < 0 or length < 0 or offset + length > len(self.data):
            raise ValueError(f'Out-of-file byte range {offset}+{length}')
        return self.data[offset:offset + length]

    def walk(self, offset, length, depth):
        if depth > 4 or (offset, length) in self.directories:
            raise ValueError('Directory cycle/repeated directory or excessive depth')
        self.directories.add((offset, length))
        buf = io.BytesIO(decompress(self.read(offset, length), self.header['internal_compression']))
        count = read_varint(buf)
        ids, last = [], 0
        for _ in range(count):
            last += read_varint(buf)
            if ids and last <= ids[-1]:
                raise ValueError('Directory tile IDs are not increasing')
            ids.append(last)
        runs = [read_varint(buf) for _ in ids]
        lengths = [read_varint(buf) for _ in ids]
        offsets = []
        for i in range(count):
            v = read_varint(buf)
            offsets.append(offsets[-1] + lengths[i-1] if i and v == 0 else v - 1)
        if buf.read(1):
            raise ValueError('Trailing directory bytes')
        for tileid, run, size, start in zip(ids, runs, lengths, offsets):
            if size <= 0 or start < 0:
                raise ValueError('Invalid directory range')
            if not run:
                if start + size > self.header['leaf_directory_length']:
                    raise ValueError('Leaf outside leaf section')
                self.walk(self.header['leaf_directory_offset'] + start, size, depth + 1)
                continue
            if start + size > self.header['tile_data_length']:
                raise ValueError('Tile outside data section')
            self.entries += 1
            self.contents.add((start, size))
            for tid in range(tileid, tileid + run):
                xyz = tileid_to_zxy(tid)
                if not self.header['min_zoom'] <= xyz[0] <= self.header['max_zoom']:
                    raise ValueError('Tile outside header zoom range')
                if xyz in self.tiles:
                    raise ValueError(f'Duplicate tile address {xyz}')
                self.tiles[xyz] = (self.header['tile_data_offset'] + start, size)

    def tile(self, xyz):
        offset, size = self.tiles[xyz]
        return decompress(self.read(offset, size), self.header['tile_compression'])


def commands(feature):
    """Check grammar before a permissive decoder can silently repair/drop rings."""
    geom, kind = feature.geometry, feature.type
    if kind not in (1, 2, 3) or not geom:
        raise ValueError('Unknown type or empty geometry commands')
    i = 0
    count_points = 0
    closed = True
    x = y = 0
    ring = []
    rings_seen = 0
    previous = None
    while i < len(geom):
        op, count = geom[i] & 7, geom[i] >> 3
        i += 1
        if count < 1 or op not in (1, 2, 7):
            raise ValueError('Invalid MVT geometry command')
        if kind == 1 and (previous is not None or op != 1):
            raise ValueError('Point must use exactly one MoveTo command')
        if kind == 2 and not ((op == 1 and previous in (None, 2)) or (op == 2 and previous == 1)):
            raise ValueError('Line must alternate MoveTo and LineTo')
        if kind == 3 and not ((op == 1 and previous in (None, 7)) or (op == 2 and previous == 1 and count > 1) or (op == 7 and previous == 2)):
            raise ValueError('Polygon must repeat MoveTo, LineTo(count>1), ClosePath')
        if op == 7:
            if kind != 3 or count != 1 or count_points < 3 or closed:
                raise ValueError('Invalid ClosePath')
            if ring[-1] == ring[0]:
                raise ValueError('Explicit duplicate closure creates zero-length segment')
            area2 = sum(a[0]*b[1]-b[0]*a[1] for a,b in zip(ring, ring[1:] + ring[:1]))
            if area2 == 0:
                raise ValueError('Zero-area quantized polygon ring')
            if rings_seen == 0 and area2 < 0:
                raise ValueError('First polygon ring has interior winding')
            rings_seen += 1
            closed = True
        else:
            if i + count * 2 > len(geom):
                raise ValueError('Truncated coordinate parameters')
            if op == 1:
                if kind == 3 and not closed:
                    raise ValueError('Unclosed polygon ring')
                if kind == 2 and count_points == 1:
                    raise ValueError('One-point line')
                if kind != 1 and count != 1:
                    raise ValueError('Non-point MoveTo count must be 1')
                count_points = count
                closed = False
                ring = []
            else:
                if not count_points or kind == 1 or closed:
                    raise ValueError('Invalid LineTo')
                count_points += count
            for j in range(count):
                dx, dy = geom[i + j*2], geom[i + j*2 + 1]
                if op == 2 and dx == 0 and dy == 0:
                    raise ValueError('Zero-length LineTo segment')
                x += (dx >> 1) ^ -(dx & 1)
                y += (dy >> 1) ^ -(dy & 1)
                ring.append((x,y))
            i += count * 2
        previous = op
    if kind == 2 and count_points < 2:
        raise ValueError('One-point line')
    if kind == 3 and not closed:
        raise ValueError('Unclosed final polygon ring')


def tile_bounds(z, x, y):
    n = 2 ** z
    return [(x / n * 360 - 180), math.degrees(math.atan(math.sinh(math.pi * (1 - 2 * (y+1)/n)))),
            ((x+1) / n * 360 - 180), math.degrees(math.atan(math.sinh(math.pi * (1 - 2*y/n))))]


def probe_archive(archive, probes, profile):
    z = archive.header['max_zoom']
    result = []
    cache = {}
    for probe in probes:
        n = 2 ** z
        wx = (probe['lon'] + 180) / 360 * n
        wy = (1 - math.asinh(math.tan(math.radians(probe['lat']))) / math.pi) / 2 * n
        best = None
        for x in range(math.floor(wx)-1, math.floor(wx)+2):
            for y in range(math.floor(wy)-1, math.floor(wy)+2):
                xyz = (z, x, y)
                if xyz not in archive.tiles:
                    continue
                if xyz not in cache:
                    cache[xyz] = TileData(archive.tile(xyz), default_options={'y_coord_down': True}).get_message()
                for layer_name in probe['layers'][profile]:
                    layer = cache[xyz].get(layer_name)
                    if not layer:
                        continue
                    extent = layer['extent']
                    source_point = Point((wx-x)*extent, (wy-y)*extent)
                    meters_per_unit = 40075016.68557849 * math.cos(math.radians(probe['lat'])) / n / extent
                    for f in layer['features']:
                        if profile == 'native' and f['id'] != probe['osm_id']:
                            continue
                        names = [v for k, v in f['properties'].items() if k == 'name' or k.startswith('name:')]
                        if probe['name'] not in names:
                            continue
                        distance = shape(f['geometry']).distance(source_point) * meters_per_unit
                        candidate = dict(distance_m=distance, tile=list(xyz), layer=layer_name,
                                         feature_id=f['id'], properties=f['properties'],
                                         tile_grid_unit_m=meters_per_unit)
                        if best is None or distance < best['distance_m']:
                            best = candidate
        tolerance = max(5, (best or {}).get('tile_grid_unit_m', 0) * 2)
        result.append(dict(probe=probe, match=best, tolerance_m=tolerance,
                           status='absent_or_profile_filtered' if best is None else
                           ('spatial_mismatch' if profile == 'native' else 'ambiguous_same_name_outside_tolerance')
                           if best['distance_m'] > tolerance else 'matched'))
    return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('archive')
    p.add_argument('--output', required=True)
    p.add_argument('--sample-per-zoom', type=int, default=32)
    p.add_argument('--all-tiles', action='store_true')
    p.add_argument('--probes')
    p.add_argument('--profile', choices=['native', 'planetiler'], default='native')
    a = p.parse_args()
    archive = Archive(a.archive)
    by_zoom = defaultdict(list)
    for xyz in archive.tiles:
        by_zoom[xyz[0]].append(xyz)
    selected = []
    coverage = {}
    for z, tiles in sorted(by_zoom.items()):
        bounds = [tile_bounds(*t) for t in tiles]
        coverage[z] = dict(tiles=len(tiles), addressed_compressed_bytes=sum(archive.tiles[t][1] for t in tiles),
                           tile_coverage_bbox=[min(b[0] for b in bounds), min(b[1] for b in bounds),
                                               max(b[2] for b in bounds), max(b[3] for b in bounds)])
        selected += tiles if a.all_tiles else sorted(tiles, key=lambda t: hashlib.sha256(str(t).encode()).digest())[:a.sample_per_zoom]
    failures, topology, inventory = [], [], defaultdict(Counter)
    issue_counts = Counter()
    topology_categories = Counter()
    topology_by_zoom_layer = Counter()
    explicit_feature_min_zooms = Counter()
    features = 0
    def issue(target, kind, xyz, layer, fid, message):
        issue_counts[kind] += 1
        if len(target) < 40:
            target.append(dict(tile=xyz, layer=layer, feature_id=fid, message=message))
    declared = {l['id']: l for l in archive.metadata.get('vector_layers', [])}
    for xyz in sorted(selected):
        try:
            decoder = TileData(archive.tile(xyz), default_options={'y_coord_down': True})
            names = [layer.name for layer in decoder.tile.layers]
            if len(names) != len(set(names)):
                raise ValueError('Duplicate layer names')
            for layer in decoder.tile.layers:
                if layer.extent <= 0 or layer.version not in (1, 2):
                    raise ValueError('Invalid MVT extent/version')
                if layer.name not in declared:
                    issue(failures, 'undeclared_layer', xyz, layer.name, None, 'Layer is absent from vector_layers metadata')
                for f in layer.features:
                    features += 1
                    inventory[f'{xyz[0]}/{layer.name}']['features'] += 1
                    inventory[f'{xyz[0]}/{layer.name}'][{1:'point',2:'line',3:'polygon'}.get(f.type, 'unknown')] += 1
                    try:
                        commands(f)
                        if len(f.tags) % 2:
                            raise ValueError('Odd tag count')
                        for ki, vi in zip(f.tags[::2], f.tags[1::2]):
                            if ki >= len(layer.keys) or vi >= len(layer.values):
                                raise ValueError('Tag dictionary reference out of range')
                            if layer.keys[ki] == 'min_zoom':
                                explicit_feature_min_zooms[str(decoder.parse_value(layer.values[vi]))] += 1
                        geom = decoder.parse_geometry(f.geometry, f.type, layer.extent, True, None)
                        shape_geom = shape(geom)
                        if shape_geom.is_empty:
                            raise ValueError('Decoded geometry is empty')
                        if not all(math.isfinite(v) for v in shape_geom.bounds):
                            raise ValueError('Nonfinite geometry')
                        if not shape_geom.is_valid:
                            components = list(shape_geom.geoms) if shape_geom.geom_type == 'MultiPolygon' else [shape_geom]
                            category = 'invalid_polygon_component' if any(not component.is_valid for component in components) else 'invalid_multipolygon_relationship'
                            topology_categories[category] += 1
                            topology_by_zoom_layer[f'{xyz[0]}/{layer.name}'] += 1
                            issue(topology, 'topology_invalid', xyz, layer.name, f.id, explain_validity(shape_geom))
                            if len(topology) < 40:
                                topology[-1]['category'] = category
                            if len(topology) <= 3:
                                topology[-1]['tile_extent'] = layer.extent
                                topology[-1]['geometry_tile_coordinates'] = geom
                                topology[-1]['properties'] = {layer.keys[k]: decoder.parse_value(layer.values[v]) for k,v in zip(f.tags[::2],f.tags[1::2])}
                    except Exception as ex:
                        issue(failures, 'feature_invalid', xyz, layer.name, f.id, str(ex))
                        if len(failures) <= 3:
                            failures[-1]['tile_extent'] = layer.extent
                            failures[-1]['geometry_commands'] = list(f.geometry)
        except Exception as ex:
            issue(failures, 'tile_invalid', xyz, None, None, str(ex))
    report = dict(archive=str(Path(a.archive).resolve()), bytes=Path(a.archive).stat().st_size,
                  header=archive.header, metadata=archive.metadata, directory_validation='passed',
                  addressed_tiles=len(archive.tiles), decoded_tiles=len(selected), decoded_features=features,
                  decode_mode='all' if a.all_tiles else f'deterministic SHA256 sample: up to {a.sample_per_zoom} per zoom',
                  coverage=coverage, decoded_layer_inventory=dict(sorted(inventory.items())),
                  issue_counts=issue_counts, structural_examples=failures, topology_examples=topology,
                  topology_categories=topology_categories, topology_by_zoom_layer=topology_by_zoom_layer,
                  explicit_feature_min_zooms=explicit_feature_min_zooms,
                  limitations=['Sampled decode does not establish validity of unexamined tiles.',
                               'GEOS checks topology in tile integer coordinates; no geometry repair applied.',
                               'Coverage boxes include tile buffers and low-zoom coarse tiles; they are not feature bounds.',
                               'Cross-profile layer/feature counts are descriptive, not equality assertions.'])
    if a.probes:
        report['source_probes'] = probe_archive(archive, json.loads(Path(a.probes).read_text(encoding='utf-8'))['probes'], a.profile)
        report['source_probe_summary'] = Counter(x['status'] for x in report['source_probes'])
    Path(a.output).write_text(json.dumps(report, indent=2, ensure_ascii=False, default=str), encoding='utf-8')
    print(json.dumps(dict(addressed_tiles=len(archive.tiles), decoded_tiles=len(selected), decoded_features=features,
                          issue_counts=issue_counts, source_probe_summary=report.get('source_probe_summary')), indent=2))
    return 1 if issue_counts else 0


if __name__ == '__main__':
    raise SystemExit(main())
