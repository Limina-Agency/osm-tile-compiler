import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

import mapbox_vector_tile
from pmtiles.tile import Compression, TileType, zxy_to_tileid
from pmtiles.writer import Writer

spec = importlib.util.spec_from_file_location('diagnostic', Path(__file__).with_name('diagnose_omt.py'))
diagnostic = importlib.util.module_from_spec(spec)
spec.loader.exec_module(diagnostic)


class DiagnosticTests(unittest.TestCase):
    def archive(self, path, invalid=False, duplicate=False, value=1):
        data = mapbox_vector_tile.encode({'name': 'poi', 'features': [
            {'id': 1, 'geometry': 'POINT(1 2)', 'properties': {'rank': 1}},
            {'id': 1 if duplicate else 2, 'geometry': 'POINT(3 4)', 'properties': {'rank': value}}]})
        if invalid:
            decoder = diagnostic.comparison.validator.TileData(data)
            decoder.tile.layers[0].features[0].geometry[:] = [2]
            data = decoder.tile.SerializeToString()
        with path.open('wb') as stream:
            writer = Writer(stream)
            writer.write_tile(zxy_to_tileid(1, 0, 0), data)
            writer.finalize({'tile_compression': Compression.NONE, 'tile_type': TileType.MVT},
                            {'vector_layers': [{'id': 'poi', 'fields': {'rank': 'Number'}}]})

    def test_raw_counts_survive_bad_first_geometry_and_later_attrs_compare(self):
        with tempfile.TemporaryDirectory() as temp:
            left, right = Path(temp) / 'left', Path(temp) / 'right'
            self.archive(left, invalid=True, value=2)
            self.archive(right, invalid=True)
            report = diagnostic.diagnose(left, right, [(1, 0, 0)])
            self.assertEqual(report['raw_totals'], {'native': 2, 'planetiler': 2})
            self.assertEqual(report['differences'], [{'layer': 'poi', 'zoom': 1, 'field': 'rank', 'count': 1}])
            self.assertEqual(report['examples'][0]['examples'][0]['id'], 2)
            self.assertEqual(len(report['decode_errors']), 2)

    def test_duplicate_pair_keys_are_reported_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'map'
            self.archive(path, duplicate=True)
            report = diagnostic.diagnose(path, path, [(1, 0, 0)])
            self.assertEqual(report['raw_totals']['native'], 2)
            self.assertEqual(report['shared_unique_label_keys'], [])
            self.assertEqual(report['duplicate_pairing_keys'][0]['native_count'], 2)

    def test_wire_types_and_coordinate_validation(self):
        with tempfile.TemporaryDirectory() as temp:
            left, right = Path(temp) / 'left', Path(temp) / 'right'
            self.archive(left, value=True)
            self.archive(right, value=1)
            report = diagnostic.diagnose(left, right, [(1, 0, 0)])
            self.assertEqual(report['differences'][0]['field'], 'rank')
            selection = Path(temp) / 'selection.json'
            selection.write_text(json.dumps({'sampling': {'coordinates': [[1, 0, 0]]}}))
            self.assertEqual(diagnostic.coordinates_from(selection), [(1, 0, 0)])
            selection.write_text(json.dumps({'coordinates': [[1, 0, 0], [1, 0, 0]]}))
            with self.assertRaisesRegex(ValueError, 'Duplicate'):
                diagnostic.coordinates_from(selection)


if __name__ == '__main__':
    unittest.main()
