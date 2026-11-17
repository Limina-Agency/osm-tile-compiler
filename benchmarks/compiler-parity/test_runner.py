import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location('runner', Path(__file__).with_name('run.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class RunnerTests(unittest.TestCase):
    def test_phase_trace_retains_counts_alongside_timers(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'stderr.log'
            events = [{'benchmark_total': 'coordinates', 'elapsed_ms': 2},
                      {'benchmark_counts': 'early_filter', 'skipped_ways': 4}]
            path.write_text('plain log\n' + '\n'.join(json.dumps(e) for e in events)
                            + '\nnull\n{"unrelated": 1}\n', encoding='utf-8')
            self.assertEqual(runner.phase_events(path), events)

    @unittest.skipUnless(os.name == 'nt', 'Windows OS counters')
    def test_windows_retained_handle_captures_short_touched_allocation(self):
        # Allocation and release happen entirely between 500ms RSS samples.
        code = "import time; time.sleep(.1); a=bytearray(32*1024*1024); a[::4096]=b'x'*(len(a)//4096); del a"
        with tempfile.TemporaryDirectory() as temp:
            result = runner.measure([getattr(sys, '_base_executable', sys.executable), '-c', code], temp, os.environ.copy(), temp, 5, .5, 0)
        counters = result['windows_process_memory']
        self.assertEqual(counters['errors'], [])
        root = next(p for p in counters['processes'] if p['is_root'])
        self.assertGreaterEqual(root['os_peak_working_set_bytes'], 32*1024*1024)
        self.assertGreaterEqual(root['os_peak_private_commit_bytes'], 32*1024*1024)
        self.assertGreater(root['os_peak_working_set_bytes'], result['sampled_peak_tree_rss_bytes'])

    def test_transient_rss_error_is_recorded_and_retried(self):
        original = runner.psutil.Process.memory_info
        calls = []
        def flaky(proc):
            calls.append(proc.pid)
            if len(calls) == 1:
                raise runner.psutil.AccessDenied(proc.pid)
            return original(proc)
        with tempfile.TemporaryDirectory() as temp, mock.patch.object(runner.psutil.Process, 'memory_info', flaky):
            result = runner.measure([getattr(sys, '_base_executable', sys.executable), '-c', 'import time; time.sleep(.2)'], temp, os.environ.copy(), temp, 5, .02, 0)
        self.assertGreater(len(calls), 1)
        self.assertEqual(result['sampling_errors'][0]['operation'], 'sample_rss')
        self.assertFalse(result['observed_process_memory_collection_complete'])
        self.assertGreater(result['sampled_peak_tree_rss_bytes'], 0)

    def test_windows_counter_failures_are_visible(self):
        from memory_counters import WindowsMemory
        counters = WindowsMemory()
        counters.enabled = True
        counters.kernel = mock.Mock()
        counters.psapi = mock.Mock()
        counters.psapi.GetProcessMemoryInfo.return_value = False
        counters.handles[(123, 1)] = 42
        counters.records[(123, 1)] = {'pid': 123}
        with mock.patch('memory_counters.ctypes.WinError', return_value=OSError('denied'), create=True), mock.patch('memory_counters.ctypes.get_last_error', return_value=5, create=True):
            result = counters.finish()
        self.assertEqual(result['errors'][0]['operation'], 'GetProcessMemoryInfo')
        self.assertNotIn('os_peak_working_set_bytes', result['processes'][0])
        counters.kernel.CloseHandle.assert_called_once_with(42)

    def test_git_fingerprint_hashes_raw_bytes_without_locale_decoding(self):
        diff = b'+name=Krak\xc3\xb3w\x81'
        outputs = [mock.Mock(returncode=0, stdout=b'abc\n'),
                   mock.Mock(returncode=0, stdout=b' M source.rs\n'),
                   mock.Mock(returncode=0, stdout=diff)]
        with mock.patch.object(runner.subprocess, 'run', side_effect=outputs) as run:
            result = runner.git_info('.')
        self.assertEqual(result['diff_sha256'], runner.hashlib.sha256(diff).hexdigest())
        self.assertEqual(result['head'], 'abc')
        self.assertNotIn('text', run.call_args.kwargs)

    def test_partial_report_cannot_claim_completed_repeats(self):
        valid = {'engine': 'native', 'success': True, 'wall_seconds': 1, 'sampled_peak_tree_rss_bytes': 10}
        partial = runner.summarize([valid], 2)
        self.assertEqual(partial['run_state'], 'running')
        self.assertEqual(partial['completed_runs'], 1)
        self.assertEqual(partial['expected_runs'], 2)
        self.assertEqual(runner.summarize([valid, valid], 2)['run_state'], 'completed')

    def test_changed_fingerprint_rejected(self):
        with mock.patch.object(runner, 'preflight', return_value={'sha256': 'new'}):
            with self.assertRaisesRegex(ValueError, 'fingerprints changed'):
                runner.verify_fingerprints({}, {'sha256': 'old'})

    def test_cleanup_survivors_are_returned(self):
        child = mock.Mock(pid=321)
        child.poll.return_value = None
        child.wait.side_effect = runner.subprocess.TimeoutExpired('child', 10)
        survivor = mock.Mock(pid=321)
        with mock.patch.object(runner.psutil, 'Process', side_effect=runner.psutil.NoSuchProcess(321)), mock.patch.object(runner.psutil, 'wait_procs', return_value=([], [survivor])):
            self.assertEqual(runner.stop_tree(child, {(321, 1): survivor}), [321])

    def test_compression_is_read_from_actual_header(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'map.pmtiles'
            header = bytearray(127)
            header[:8] = b'PMTiles\x03'
            header[98] = 2
            path.write_bytes(header)
            self.assertEqual(runner.tile_compression(path), 'gzip')
            header[98] = 1
            path.write_bytes(header)
            self.assertEqual(runner.tile_compression(path), 'none')
            path.write_bytes(b'bad')
            with self.assertRaisesRegex(ValueError, 'Invalid PMTiles'):
                runner.tile_compression(path)

    def test_phase_events_include_aggregates_without_treating_logs_as_json(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'stderr.log'
            path.write_text('ordinary log\n{"benchmark_phase":"relations","event":"begin"}\n'
                            '{"benchmark_total":"relation_build","elapsed_ms":0.125,"count":2}\n'
                            '{"unrelated":true}\n')
            events = runner.phase_events(path)
            self.assertEqual(len(events), 2)
            self.assertEqual(events[1]['elapsed_ms'], .125)

    def test_nonfinite_and_boolean_configuration_rejected(self):
        base = dict(threads=1, repeats=1, timeout_seconds=1, sample_interval_seconds=.1, rss_limit_gib=0)
        for key, value in [('threads', True), ('repeats', False), ('timeout_seconds', float('nan')),
                           ('sample_interval_seconds', float('inf')), ('rss_limit_gib', float('nan'))]:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                runner.preflight(dict(base, **{key: value}))

    def test_matched_omt_is_blocked_without_running_or_preflight(self):
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / 'blocked'
            self.assertEqual(runner.run({}, output, 'matched-omt'), 2)
            report = json.loads((output / 'report.json').read_text())
            self.assertFalse(report['matched_work'])
            self.assertEqual(report['status'], 'blocked')

    def test_existing_output_never_overwritten(self):
        with tempfile.TemporaryDirectory() as temp:
            with self.assertRaisesRegex(ValueError, 'already exists'):
                runner.run({}, Path(temp), 'matched-omt')

    def test_exit_failure_is_captured(self):
        with tempfile.TemporaryDirectory() as temp:
            result = runner.measure([sys.executable, '-c', 'import sys; print("failure"); sys.exit(7)'],
                                    temp, os.environ.copy(), temp, 10, .01, 0)
            self.assertEqual(result['exit_code'], 7)
            self.assertIsNone(result['stop_reason'])
            self.assertIn('failure', (Path(temp) / 'stdout.log').read_text())

    def test_timeout_kills_descendant_and_counts_its_rss(self):
        with tempfile.TemporaryDirectory() as temp:
            command = [sys.executable, '-c',
                       'import subprocess,sys,time; p=subprocess.Popen([sys.executable,"-c","import time; a=bytearray(32*1024*1024); time.sleep(30)"]); print(p.pid,flush=True); time.sleep(30)']
            result = runner.measure(command, temp, os.environ.copy(), temp, 1.5, .02, 0)
            self.assertEqual(result['stop_reason'], 'timeout')
            self.assertGreater(result['sampled_peak_tree_rss_bytes'], 32 * 1024**2)
            pid = int((Path(temp) / 'stdout.log').read_text().strip())
            self.assertFalse(runner.psutil.pid_exists(pid))
            self.assertLess(result['wall_seconds'], 10)

    def test_memory_limit_stops_process(self):
        with tempfile.TemporaryDirectory() as temp:
            result = runner.measure([sys.executable, '-c', 'import time; time.sleep(30)'],
                                    temp, os.environ.copy(), temp, 10, .01, 1)
            self.assertEqual(result['stop_reason'], 'rss_limit')

    def test_failed_samples_excluded_from_summary(self):
        result = runner.summarize([{'engine': 'native', 'success': False, 'wall_seconds': 1,
                                    'sampled_peak_tree_rss_bytes': 10}])
        self.assertEqual(result['engines'], {})
        self.assertFalse(result['matched_work'])

    def test_preflight_fingerprints_only_the_source_archives(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            config = dict(threads=1, repeats=1, timeout_seconds=1, sample_interval_seconds=.1,
                          rss_limit_gib=0, planetiler_sources=temp)
            for key in ['input_pbf', 'native_binary', 'java_binary', 'planetiler_jar']:
                path = root / key
                path.write_bytes(b'test')
                config[key] = str(path)
            for name in runner.SOURCE_ARCHIVES:
                (root / name).write_bytes(b'source')
            before = runner.preflight(config)['planetiler_sources']
            # The native compiler's extraction cache appears on its first run.
            (root / '.extracted').mkdir()
            (root / '.extracted' / 'natural_earth_vector.sqlite').write_bytes(b'cache')
            self.assertEqual(runner.preflight(config)['planetiler_sources'], before)
            self.assertEqual([Path(p['path']).name for p in before], runner.SOURCE_ARCHIVES)

    def test_preflight_hash_mismatch_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            config = dict(threads=1, repeats=1, timeout_seconds=1, sample_interval_seconds=.1,
                          rss_limit_gib=0, planetiler_sources=temp, expected_sha256={'input_pbf': 'wrong'})
            for key in ['input_pbf', 'native_binary', 'java_binary', 'planetiler_jar']:
                path = root / key
                path.write_bytes(b'test')
                config[key] = str(path)
            for name in ['lake_centerline.shp.zip', 'natural_earth_vector.sqlite.zip', 'water-polygons-split-3857.zip']:
                (root / name).write_bytes(b'source')
            with self.assertRaisesRegex(ValueError, 'SHA256 mismatch'):
                runner.preflight(config)


if __name__ == '__main__':
    unittest.main()
