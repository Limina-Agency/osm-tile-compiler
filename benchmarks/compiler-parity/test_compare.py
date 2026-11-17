"""Small synthetic archives only; no regional data or benchmark compilation."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

import mapbox_vector_tile
from pmtiles.writer import Writer
from pmtiles.tile import Compression, TileType, zxy_to_tileid

spec = importlib.util.spec_from_file_location('compare', Path(__file__).with_name('compare.py'))
compare = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = compare  # multiprocessing spawn must import its worker functions.
spec.loader.exec_module(compare)


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)

    def archive(self, name, value=1, coords=((1, 0, 0),), metadata_extra=None, reverse=False, invalid=False, topology=False, geometry=None):
        path = Path(self.tmp.name) / name
        features = [{'geometry': 'POINT(10 20)', 'properties': {'value': value}, 'id': 1},
                    {'geometry': 'POINT(30 40)', 'properties': {'value': value}, 'id': 2}]
        if reverse:
            features.reverse()
        if topology:
            features[0]['geometry'] = 'MULTIPOLYGON(((0 0,0 10,10 10,10 0,0 0)),((5 5,5 15,15 15,15 5,5 5)))'
        if geometry:
            features[0]['geometry'] = geometry
        data = mapbox_vector_tile.encode({'name': 'test', 'features': features})
        if invalid:
            decoder = compare.validator.TileData(data)
            decoder.tile.layers[0].features[0].geometry[:] = [2]
            data = decoder.tile.SerializeToString()
        metadata = {'vector_layers': [{'id': 'test', 'fields': {'value': 'Number'}}], **(metadata_extra or {})}
        with path.open('wb') as stream:
            writer = Writer(stream)
            for xyz in coords:
                writer.write_tile(zxy_to_tileid(*xyz), data)
            writer.finalize({'tile_compression': Compression.NONE, 'tile_type': TileType.MVT}, metadata)
        return path

    def manifests(self, path=None):
        return {'input_sha256': 'a'*64, 'schema': 'test-v1', 'profile': 'test',
                'artifact_sha256': compare.digest(path) if path else None,
                'settings': {'minzoom': 1, 'maxzoom': 1, 'detailzoom': 1,
                             'bounds_policy': 'source-derived', 'tile_compression': 'none',
                             'output': 'pmtiles-mvt', 'annotations': 'none', 'auxiliary_inputs': {}}}

    def test_identity_full(self):
        path = self.archive('a')
        result, code = compare.compare(path, path, all_tiles=True)
        self.assertEqual(code, 0)
        self.assertTrue(result['whole_archive_semantic_parity'])

    def test_order_and_metadata_producer_are_not_semantics(self):
        a, b = self.archive('a'), self.archive('b', reverse=True, metadata_extra={'name': 'other producer'})
        result, code = compare.compare(a, b, self.manifests(a), self.manifests(b))
        self.assertEqual(code, 0)
        self.assertFalse(result['byte_identical'])
        self.assertFalse(result['whole_archive_semantic_parity'])

    def test_typed_attributes(self):
        a, b = self.archive('a'), self.archive('b', value=True)
        result, code = compare.compare(a, b, self.manifests(a), self.manifests(b))
        self.assertEqual(code, 1)
        self.assertEqual(result['status'], 'different')

    def test_missing_tile_complete_coverage_even_small_sample(self):
        a, b = self.archive('a'), self.archive('b', coords=((1, 0, 0), (1, 1, 0)))
        result, code = compare.compare(a, b, self.manifests(a), self.manifests(b), count=1)
        self.assertEqual(code, 1)
        self.assertEqual(result['coverage'][1]['right_only'], 1)

    def test_union_selection_is_symmetric(self):
        a, b = {(1, 0, 0), (2, 1, 0)}, {(1, 1, 0), (2, 0, 0)}
        self.assertEqual(compare.select_coordinates(a, b, 1), compare.select_coordinates(b, a, 1))
        self.assertEqual(set(compare.select_coordinates(a, b, 1, True)), a | b)

    def test_unequal_profile_gate(self):
        a, b = self.archive('a'), self.archive('b', metadata_extra={'name': 'planetiler'})
        lm, rm = self.manifests(a), self.manifests(b)
        lm['profile'], rm['profile'] = 'Tactical', 'OMT'
        result, code = compare.compare(a, b, lm, rm)
        self.assertEqual(code, 1)
        self.assertIn('different_profile', result['gate']['reasons'])
        self.assertEqual(result['gate']['native_omt_support'], 'native experimental profile available; parity not asserted')

    def test_missing_provenance_is_inconclusive(self):
        a, b = self.archive('a'), self.archive('b', reverse=True)
        result, code = compare.compare(a, b)
        self.assertEqual(code, 1)
        self.assertIn('missing_manifest_input_sha256', result['gate']['reasons'])

    def test_invalid_geometry_fails_identity(self):
        a = self.archive('a', invalid=True)
        result, code = compare.compare(a, a)
        self.assertEqual(code, 2)
        self.assertTrue(result['errors'])

    def test_invalid_topology_keeps_semantics_and_inventory(self):
        a = self.archive('a', topology=True)
        result, code = compare.compare(a, a, all_tiles=True)
        self.assertEqual(code, 2)
        self.assertTrue(result['semantic_equal_on_sample'])
        self.assertFalse(result['sample_geometry_valid'])
        self.assertFalse(result['whole_archive_semantic_parity'])
        self.assertEqual(result['sample_inventory']['left']['1/test/features'], 2)
        self.assertFalse(result['structural_errors'])
        self.assertEqual(len(result['geometry_validity_issues']), 2)

    def test_missing_policy_and_auxiliary_inputs_block_gate(self):
        a, b = self.archive('a'), self.archive('b', reverse=True)
        manifest = self.manifests()
        del manifest['settings']['annotations']
        del manifest['settings']['auxiliary_inputs']
        result, code = compare.compare(a, b, manifest, manifest)
        self.assertEqual(code, 1)
        self.assertIn('missing_settings_auxiliary_inputs', result['gate']['reasons'])

    def test_stale_manifest_cannot_pass(self):
        a, b = self.archive('a'), self.archive('b', reverse=True)
        result, code = compare.compare(a, b, self.manifests(a), self.manifests(a))
        self.assertEqual(code, 1)
        self.assertIn('right_missing_or_mismatched_artifact_sha256', result['gate']['reasons'])

    def test_false_compression_provenance_cannot_pass(self):
        a = self.archive('a')
        manifest = self.manifests(a)
        manifest['settings']['tile_compression'] = 'gzip'
        result, code = compare.compare(a, a, manifest, manifest)
        self.assertEqual(code, 1)
        self.assertIn('left_mismatched_tile_compression', result['gate']['reasons'])

    def test_second_archive_failure_closes_first(self):
        a = self.archive('a')
        b = Path(self.tmp.name) / 'empty'
        b.write_bytes(b'')
        with self.assertRaises(Exception):
            compare.compare(a, b)
        # Windows cannot unlink mapped archives, including partially opened ones.
        a.unlink()
        b.unlink()

    def test_duplicate_metadata_layers_rejected_and_closed(self):
        a = self.archive('a', metadata_extra={'vector_layers': [{'id': 'test'}, {'id': 'test'}]})
        with self.assertRaisesRegex(ValueError, 'duplicate'):
            compare.compare(a, a)
        a.unlink()

    def test_nonobject_manifest_rejected(self):
        a = self.archive('a')
        with self.assertRaisesRegex(ValueError, 'JSON objects'):
            compare.compare(a, a, [], [])

    def test_line_direction_is_semantic(self):
        a = self.archive('a', geometry='LINESTRING(0 0,10 10)')
        b = self.archive('b', geometry='LINESTRING(10 10,0 0)')
        result, code = compare.compare(a, b, self.manifests(a), self.manifests(b))
        self.assertEqual(code, 1)
        self.assertFalse(result['semantic_equal_on_sample'])

    def test_parallel_matches_sequential_for_difference_coverage_and_validity(self):
        a = self.archive('a', coords=((1, 0, 0), (1, 1, 0)), topology=True)
        b = self.archive('b', coords=((1, 0, 0),), value=2)
        args = (a, b, self.manifests(a), self.manifests(b))
        sequential = compare.compare(*args, all_tiles=True, workers=1)
        parallel = compare.compare(*args, all_tiles=True, workers=2)
        self.assertEqual(parallel, sequential)
        self.assertEqual(parallel[1], 2)
        a.unlink()
        b.unlink()

    def test_parallel_structural_failure_matches_sequential(self):
        a = self.archive('a', invalid=True)
        b = self.archive('b')
        self.assertEqual(compare.compare(a, b, workers=2), compare.compare(a, b, workers=1))

    def test_parallel_rejects_malformed_archive_without_leaking_handles(self):
        a = self.archive('a')
        b = Path(self.tmp.name) / 'empty'
        b.write_bytes(b'')
        with self.assertRaises(Exception):
            compare.compare(a, b, workers=2)
        a.unlink()
        b.unlink()

    def test_workers_must_be_positive(self):
        a = self.archive('a')
        with self.assertRaisesRegex(ValueError, 'workers must be positive'):
            compare.compare(a, a, workers=0)


if __name__ == '__main__':
    unittest.main()
