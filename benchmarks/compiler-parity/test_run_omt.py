import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('omt_runner', Path(__file__).with_name('run_omt.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class OmtRunnerTests(unittest.TestCase):
    def config(self, root):
        for name in ['input.pbf', 'compile_omt.exe', 'java.exe', 'planetiler.jar']:
            (root / name).write_bytes(name.encode())
        sources = root / 'sources'
        sources.mkdir()
        for name in ['lake_centerline.shp.zip', 'natural_earth_vector.sqlite.zip', 'water-polygons-split-3857.zip']:
            (sources / name).write_bytes(name.encode())
        return {'input_pbf': str(root / 'input.pbf'), 'native_binary': str(root / 'compile_omt.exe'),
                'java_binary': str(root / 'java.exe'), 'planetiler_jar': str(root / 'planetiler.jar'),
                'planetiler_sources': str(sources), 'java_heap': '4g', 'threads': 4, 'repeats': 1,
                'bounds': [18.0, 49.0, 21.0, 51.0], 'timeout_seconds': 10,
                'sample_interval_seconds': .1, 'rss_limit_gib': 1}

    def test_commands_use_native_entry_and_same_explicit_workload_options(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            config = self.config(root)
            native, planetiler = runner.commands(config, root / 'out')
            self.assertEqual(native[1][0], config['native_binary'])
            self.assertEqual(native[1][3], config['planetiler_sources'])
            i = native[1].index('--bounds')
            self.assertEqual(native[1][i+1:i+5], ['18.0', '49.0', '21.0', '51.0'])
            self.assertIn('--bounds=18.0,49.0,21.0,51.0', planetiler[1])
            self.assertIn('--tile-write-threads=1', planetiler[1])
            self.assertIn('--use-wikidata=false', planetiler[1])
            self.assertIn('--download=false', planetiler[1])
            self.assertNotIn('--native-zoom', native[1])

    def test_profile_and_old_native_entry_guard(self):
        with tempfile.TemporaryDirectory() as temp:
            config = self.config(Path(temp))
            with self.assertRaisesRegex(ValueError, 'parity'):
                runner.validate_config(dict(config, profile='full-omt'))
            with self.assertRaisesRegex(ValueError, 'Tactical'):
                runner.validate_config(dict(config, native_binary='compile-region-after.exe'))
            for bounds in [[0, 1, 0, 2], [0, 1, 2, float('nan')], [0, False, 2, 3], [0, 1, 2]]:
                with self.assertRaises(ValueError):
                    runner.validate_config(dict(config, bounds=bounds))

    def test_sequential_results_keep_parity_unverified_and_bound_hashes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            config = self.config(root)
            output = root / 'out'
            calls = []
            def measure(command, cwd, env, destination, *args):
                calls.append(Path(destination).name)
                header = bytearray(127)
                header[:8] = b'PMTiles\x03'
                header[98] = 2
                (destination / 'basemap.pmtiles').write_bytes(header)
                (destination / 'stderr.log').write_text('')
                (destination / 'stdout.log').write_text('')
                if destination.name == 'native':
                    (destination / 'compile-result.json').write_text(json.dumps({'schema': 'OpenMapTiles 3.16.0 (native experimental port)', 'profile_revision': runner.REVISION, 'parity_verified': False}))
                return {'command': command, 'exit_code': 0, 'stop_reason': None, 'wall_seconds': 1, 'sampled_peak_tree_rss_bytes': 100}
            with mock.patch.object(runner.common, 'measure', side_effect=measure):
                self.assertEqual(runner.run(config, output), 0)
            self.assertEqual(calls, ['native', 'planetiler'])
            report = json.loads((output / 'report.json').read_text())
            self.assertEqual(report['run_state'], 'completed')
            self.assertFalse(report['matched_work'])
            self.assertFalse(report['parity_verified'])
            left = json.loads((output / 'repeat-01/native/comparison-manifest.json').read_text())
            right = json.loads((output / 'repeat-01/planetiler/comparison-manifest.json').read_text())
            self.assertEqual(left['schema'], right['schema'])
            self.assertNotEqual(left['profile'], right['profile'])
            self.assertEqual(left['settings'], right['settings'])
            self.assertEqual(len(left['artifact_sha256']), 64)
            self.assertEqual(len(left['settings']['auxiliary_inputs']), 3)

    def test_native_only_plan_and_expected_completion(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            config = self.config(root)
            config['repeats'] = 2
            self.assertEqual([x[0] for x in runner.commands(config, root, 'native')], ['native'])
            self.assertEqual([x[0] for x in runner.commands(config, root, 'planetiler')], ['planetiler'])
            with mock.patch.object(runner.common, 'measure') as measure:
                self.assertEqual(runner.run(config, root / 'out', True, 'native'), 0)
                measure.assert_not_called()
            manifest = json.loads((root / 'out/manifest.json').read_text())
            self.assertEqual(manifest['engine_selection'], 'native')
            self.assertEqual([p[0][0] for p in manifest['plans']], ['native', 'native'])
            expected = sum(len(plan) for plan in manifest['plans'])
            runs = [dict(engine='native', success=True, wall_seconds=1, sampled_peak_tree_rss_bytes=1) for _ in range(2)]
            self.assertEqual(runner.summarize(runs, expected)['run_state'], 'completed')

    def test_dry_run_does_not_launch(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            config = self.config(root)
            with mock.patch.object(runner.common, 'measure') as measure:
                self.assertEqual(runner.run(config, root / 'out', True), 0)
                measure.assert_not_called()


if __name__ == '__main__':
    unittest.main()
