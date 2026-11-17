"""Sequential compiler diagnostics. Requires psutil; never downloads or builds tools."""
import argparse
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time

import psutil
from memory_counters import WindowsMemory

BLOCKED = 'Native TacticalV7 is not a complete OpenMapTiles implementation; matched OMT work is unsupported.'
LIMITATIONS = ('RSS is sampled process-tree resident memory, may miss short peaks and double-count shared pages. '
              'Run on the same OS as the compiler (not a Windows wrapper around WSL). '
              'Page cache is uncontrolled; no cold-cache claim. Configured threads are not CPU affinity. '
              'Phase logs are engine-specific and cannot establish equal phase work.')


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2) + '\n', encoding='utf-8')


SOURCE_ARCHIVES = ['lake_centerline.shp.zip', 'natural_earth_vector.sqlite.zip', 'water-polygons-split-3857.zip']


def fingerprint(path):
    path = Path(path).resolve(strict=True)
    digest = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return {'path': str(path), 'bytes': path.stat().st_size, 'sha256': digest.hexdigest()}


def git_info(repo):
    def git(*args):
        result = subprocess.run(['git', '-C', str(repo), *args], capture_output=True)
        return result.stdout.strip() if result.returncode == 0 else None
    head, status = git('rev-parse', 'HEAD'), git('status', '--porcelain')
    return {'head': head.decode('utf-8', errors='replace') if head is not None else None,
            'status': status.decode('utf-8', errors='replace') if status is not None else None,
            'diff_sha256': hashlib.sha256(git('diff', 'HEAD') or b'').hexdigest()}


def stop_tree(child, known):
    try:
        root = psutil.Process(child.pid)
        known.update({(p.pid, p.create_time()): p for p in root.children(recursive=True)})
        known[(root.pid, root.create_time())] = root
    except psutil.Error:
        pass
    for proc in reversed(list(known.values())):
        try:
            proc.kill()
        except psutil.Error:
            pass
    try:
        child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    _, alive = psutil.wait_procs(list(known.values()), timeout=5)
    return sorted({proc.pid for proc in alive} | ({child.pid} if child.poll() is None else set()))


def measure(command, cwd, env, destination, timeout, interval, rss_limit):
    """Track known descendants even after reparenting; persist each sample to JSONL."""
    destination = Path(destination)
    known = {}
    memory = WindowsMemory()
    sampling_errors = []
    peak = samples = 0
    reason = None
    cleanup_survivors = []
    started = datetime.datetime.now(datetime.timezone.utc).isoformat()
    start = time.perf_counter()
    with (destination / 'stdout.log').open('wb') as stdout, (destination / 'stderr.log').open('wb') as stderr, (destination / 'rss.jsonl').open('w') as timeline:
        child = subprocess.Popen(command, cwd=cwd, env=env, stdout=stdout, stderr=stderr)
        root_key = (child.pid, None)
        try:
            memory.attach(root_key, child)
            try:
                root = psutil.Process(child.pid)
                known[(root.pid, root.create_time())] = root
            except psutil.NoSuchProcess:
                pass
            while True:
                for proc in list(known.values()):
                    try:
                        for descendant in proc.children(recursive=True):
                            key = (descendant.pid, descendant.create_time())
                            known[key] = descendant
                            memory.attach(key)
                    except psutil.Error as error:
                        if not isinstance(error, psutil.NoSuchProcess):
                            sampling_errors.append({'operation': 'discover_children', 'pid': proc.pid, 'error': str(error)})
                memory.sample()
                rss = alive = 0
                for key, proc in list(known.items()):
                    try:
                        if not proc.is_running() or proc.status() == psutil.STATUS_ZOMBIE:
                            known.pop(key, None)
                            continue
                        rss += proc.memory_info().rss
                        alive += 1
                    except psutil.Error as error:
                        if not isinstance(error, psutil.NoSuchProcess):
                            sampling_errors.append({'operation': 'sample_rss', 'pid': proc.pid, 'error': str(error)})
                            alive += 1  # retry transient access/query failures next sample
                        else:
                            known.pop(key, None)
                elapsed = time.perf_counter() - start
                peak = max(peak, rss)
                samples += 1
                timeline.write(json.dumps({'elapsed_seconds': elapsed, 'rss_bytes': rss, 'processes': alive}) + '\n')
                timeline.flush()
                if child.poll() is not None and not alive:
                    break
                if elapsed >= timeout:
                    reason = 'timeout'
                elif rss_limit and rss > rss_limit:
                    reason = 'rss_limit'
                if reason:
                    cleanup_survivors = stop_tree(child, known)
                    break
                time.sleep(interval)
        except BaseException:
            stop_tree(child, known)
            memory.finish()
            raise
        exit_code = child.poll() if cleanup_survivors else child.wait()
        windows_memory = memory.finish()
    return {'command': command, 'cwd': str(cwd), 'started_utc': started,
            'wall_seconds': time.perf_counter() - start, 'exit_code': exit_code,
            'stop_reason': reason, 'sampled_peak_tree_rss_bytes': peak,
            'windows_process_memory': windows_memory, 'sampling_errors': sampling_errors,
            'memory_collection_had_errors': bool(sampling_errors or windows_memory['errors']),
            'observed_process_memory_collection_complete': not bool(sampling_errors or windows_memory['errors']),
            'sample_count': samples, 'sample_interval_seconds': interval,
            'cleanup_survivor_pids': cleanup_survivors,
            'wall_time_scope': 'Includes process launch, exit detection sampling lag and, on termination, cleanup. Failed-run timings are not performance samples.'}


def commands(config, directory):
    native = directory / 'native'
    planetiler = directory / 'planetiler'
    return [
        ('native', [config['native_binary'], config['input_pbf'], str(native), '--native-zoom', '14', '--detail-zoom', '14']),
        ('planetiler', [config['java_binary'], '-Xmx' + config['java_heap'], '-jar', config['planetiler_jar'],
                       '--osm-path=' + config['input_pbf'], '--download-dir=' + config['planetiler_sources'],
                       '--output=' + str(planetiler / 'basemap.pmtiles'), '--tmpdir=' + str(planetiler / 'tmp'),
                       '--threads=' + str(config['threads']), '--minzoom=0', '--maxzoom=14', '--render-maxzoom=14'])]


def phase_events(path):
    events = []
    with path.open(encoding='utf-8', errors='replace') as stream:
        for line in stream:
            try:
                event = json.loads(line)
                if isinstance(event, dict) and any(key in event for key in ('benchmark_phase', 'benchmark_total', 'benchmark_counts')):
                    events.append(event)
            except json.JSONDecodeError:
                pass
    return events


def preflight(config):
    for key in ['threads', 'repeats', 'timeout_seconds', 'sample_interval_seconds']:
        if isinstance(config[key], bool) or not isinstance(config[key], (int, float)) or not math.isfinite(config[key]) or config[key] <= 0:
            raise ValueError(key + ' must be positive')
    if not math.isfinite(config['rss_limit_gib']) or config['rss_limit_gib'] < 0:
        raise ValueError('rss_limit_gib must be nonnegative')
    if not isinstance(config['repeats'], int) or not isinstance(config['threads'], int):
        raise ValueError('threads and repeats must be integers')
    records = {key: fingerprint(config[key]) for key in ['input_pbf', 'native_binary', 'java_binary', 'planetiler_jar']}
    sources = Path(config['planetiler_sources'])
    for name in SOURCE_ARCHIVES:
        if not (sources / name).is_file():
            raise ValueError('Missing offline Planetiler source: ' + name)
    # Only the archives both compilers read: the native compiler also keeps
    # its download records and an extraction cache in this directory, and
    # creates the cache on its first run.
    records['planetiler_sources'] = [fingerprint(sources / name) for name in SOURCE_ARCHIVES]
    for key, expected in config.get('expected_sha256', {}).items():
        if key not in records or records[key]['sha256'].lower() != expected.lower():
            raise ValueError('SHA256 mismatch: ' + key)
    return records


def verify_fingerprints(config, expected):
    if preflight(config) != expected:
        raise ValueError('Input or tool fingerprints changed since preflight; results cannot be attributed to the original manifest')


def tile_compression(path):
    with Path(path).open('rb') as stream:
        header = stream.read(127)
    if len(header) != 127 or header[:8] != b'PMTiles\x03':
        raise ValueError('Invalid PMTiles v3 header: ' + str(path))
    return {0: 'unknown', 1: 'none', 2: 'gzip', 3: 'brotli', 4: 'zstd'}.get(header[98], 'unknown')


def run(config, output, profile='diagnostic', dry_run=False):
    if output.exists():
        raise ValueError('Output directory already exists; choose a new directory')
    if profile == 'matched-omt':
        output.mkdir(parents=True)
        write_json(output / 'report.json', {'status': 'blocked', 'matched_work': False, 'reason': BLOCKED})
        return 2
    inputs = preflight(config)
    # Normalize paths before changing subprocess working directories.
    config = dict(config)
    for key in ['input_pbf', 'native_binary', 'java_binary', 'planetiler_jar', 'planetiler_sources']:
        config[key] = str(Path(config[key]).resolve())
    output.mkdir(parents=True)
    manifest = {'format_version': 1, 'profile': profile, 'matched_work': False, 'reason': BLOCKED,
                'config': config, 'files': inputs, 'harness_checkout': git_info(Path(__file__).resolve().parents[2]),
                'harness_script': fingerprint(__file__),
                'native_build_provenance': {'asserted_revision': config.get('native_build_revision'),
                                            'verification': 'User-supplied assertion; not inferred from the harness checkout. Binary SHA256 is recorded separately.'},
                'host': {'platform': platform.platform(), 'python': sys.version, 'psutil': psutil.__version__,
                         'logical_cpus': psutil.cpu_count(), 'memory_bytes': psutil.virtual_memory().total},
                'limitations': LIMITATIONS,
                'inherited_environment': {k: os.environ[k] for k in ['JAVA_TOOL_OPTIONS', '_JAVA_OPTIONS', 'JDK_JAVA_OPTIONS', 'RUST_LOG', 'CARGO_TARGET_DIR'] if k in os.environ},
                'plans': [commands(config, output / ('repeat-%02d' % i)) for i in range(1, config['repeats'] + 1)]}
    write_json(output / 'manifest.json', manifest)
    if dry_run:
        write_json(output / 'report.json', {'status': 'dry_run', 'matched_work': False, 'reason': BLOCKED})
        return 0
    runs = []
    expected_runs = config['repeats'] * 2
    write_json(output / 'report.json', summarize(runs, expected_runs))
    for index, plan in enumerate(manifest['plans'], 1):
        # Alternate engine order to reduce systematic order bias; never concurrent.
        for engine, command in (plan if index % 2 else list(reversed(plan))):
            directory = output / ('repeat-%02d' % index) / engine
            directory.mkdir(parents=True)
            try:
                verify_fingerprints(config, inputs)
            except (ValueError, OSError) as error:
                report = summarize(runs, expected_runs)
                report.update(status='failed', run_state='failed', provenance_error=str(error))
                write_json(output / 'report.json', report)
                return 1
            env = dict(os.environ, RAYON_NUM_THREADS=str(config['threads']))
            try:
                result = measure(command, directory, env, directory, config['timeout_seconds'],
                                 config['sample_interval_seconds'], config['rss_limit_gib'] * 1024**3)
            except OSError as error:
                result = {'command': command, 'cwd': str(directory), 'exit_code': None,
                          'stop_reason': 'launch_error', 'error': str(error), 'wall_seconds': None,
                          'sampled_peak_tree_rss_bytes': None}
            result.update(engine=engine, repeat=index)
            try:
                verify_fingerprints(config, inputs)
            except (ValueError, OSError) as error:
                result['provenance_error'] = str(error)
            result['artifacts'] = [fingerprint(p) for p in sorted(directory.glob('*.pmtiles'))]
            result['phase_trace'] = {'stdout': str(directory / 'stdout.log'), 'stderr': str(directory / 'stderr.log'),
                                     'native_result': str(directory / 'compile-result.json') if engine == 'native' else None,
                                     'events': phase_events(directory / 'stderr.log'),
                                     'clock_note': 'Native events use an internal first-phase clock, not the harness process-start clock. No exact RSS phase alignment is claimed.'}
            expected = ['basemap.pmtiles']
            if engine == 'native':
                expected += ['basemap.annotations.pmtiles', 'compile-result.json']
            result['missing_artifacts'] = [name for name in expected if not (directory / name).is_file()]
            compression = None
            if (directory / 'basemap.pmtiles').is_file():
                try:
                    compression = tile_compression(directory / 'basemap.pmtiles')
                except ValueError as error:
                    result['artifact_error'] = str(error)
            result['success'] = (result['exit_code'] == 0 and result['stop_reason'] is None and not result['missing_artifacts']
                                 and not result.get('cleanup_survivor_pids') and not result.get('provenance_error') and not result.get('artifact_error'))
            write_json(directory / 'measurement.json', result)
            write_json(directory / 'comparison-manifest.json', {'input_sha256': inputs['input_pbf']['sha256'],
                       'artifact_sha256': next((item['sha256'] for item in result['artifacts'] if Path(item['path']).name == 'basemap.pmtiles'), None),
                       'schema': 'TacticalV7' if engine == 'native' else 'OpenMapTiles3.16.0',
                       'profile': 'tactical' if engine == 'native' else 'full-omt',
                       'settings': {'minzoom': 0, 'maxzoom': 14, 'detailzoom': 14, 'threads': config['threads'],
                                    'output': 'pmtiles-mvt', 'annotations': 'companion-bincode-v2' if engine == 'native' else 'none',
                                    'bounds_policy': 'no-explicit-bounds;source-derived', 'tile_compression': compression,
                                    'auxiliary_inputs': {} if engine == 'native' else {Path(p['path']).name: p['sha256'] for p in inputs['planetiler_sources']}}})
            runs.append(result)
            write_json(output / 'report.json', summarize(runs, expected_runs))
            if not result['success']:
                return 1
    return 0


def summarize(runs, expected_runs=None):
    complete = expected_runs is not None and len(runs) == expected_runs
    summary = {'status': 'failed' if any(not r['success'] for r in runs) else 'diagnostic_only',
               'run_state': 'failed' if any(not r['success'] for r in runs) else 'completed' if complete else 'running',
               'expected_runs': expected_runs, 'completed_runs': len(runs),
               'matched_work': False, 'reason': BLOCKED, 'runs': runs, 'engines': {}}
    for engine in ['native', 'planetiler']:
        valid = [r for r in runs if r['engine'] == engine and r['success']]
        if valid:
            summary['engines'][engine] = {metric: {'median': statistics.median(r[metric] for r in valid),
                'min': min(r[metric] for r in valid), 'max': max(r[metric] for r in valid)}
                for metric in ['wall_seconds', 'sampled_peak_tree_rss_bytes']}
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=Path(__file__).with_name('example-config.json'))
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--profile', choices=['diagnostic', 'matched-omt'], default='diagnostic')
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    try:
        return run(json.loads(args.config.read_text(encoding='utf-8')), args.output.resolve(), args.profile, args.dry_run)
    except (ValueError, OSError, KeyError) as error:
        print(str(error), file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
