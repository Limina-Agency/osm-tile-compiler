"""Sequential experimental native OMT / pinned Planetiler measurements.

This runner never claims profile parity. No builds or downloads are performed.
"""
import argparse
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import sys

spec = importlib.util.spec_from_file_location('diagnostic_runner', Path(__file__).with_name('run.py'))
common = importlib.util.module_from_spec(spec)
spec.loader.exec_module(common)

REVISION = '91516fdf477915b1a985015811049b9b96c7f87e'
REASON = 'Native experimental OMT source profile is implemented but semantic parity with the pinned full OMT profile is unverified.'


def commands(config, directory, engine="both"):
    bounds = [str(v) for v in config['bounds']]
    if engine not in {"both", "native", "planetiler"}:
        raise ValueError("engine must be both, native, or planetiler")
    plans = [
        ('native', [config['native_binary'], config['input_pbf'], str(directory / 'native'),
                    config['planetiler_sources'], '--bounds', *bounds, '--threads', str(config['threads']),
                    '--min-zoom', '0', '--max-zoom', '14']),
        ('planetiler', [config['java_binary'], '-Xmx' + config['java_heap'], '-jar', config['planetiler_jar'],
                       '--osm-path=' + config['input_pbf'], '--download-dir=' + config['planetiler_sources'],
                       '--output=' + str(directory / 'planetiler' / 'basemap.pmtiles'),
                       '--tmpdir=' + str(directory / 'planetiler' / 'tmp'),
                       '--bounds=' + ','.join(bounds), '--threads=' + str(config['threads']),
                       '--minzoom=0', '--maxzoom=14', '--render-maxzoom=14', '--tile-write-threads=1',
                       '--download=false', '--fetch-wikidata=false', '--use-wikidata=false'])]
    return [plan for plan in plans if engine == 'both' or plan[0] == engine]


def validate_config(config):
    bounds = config.get('bounds')
    if not isinstance(bounds, list) or len(bounds) != 4 or any(isinstance(v, bool) or not isinstance(v, (int, float)) or not math.isfinite(v) for v in bounds):
        raise ValueError('bounds must be finite [west, south, east, north]')
    w, s, e, n = bounds
    if not (-180 <= w < e <= 180 and -85.051129 <= s < n <= 85.051129):
        raise ValueError('bounds must be non-wrapping Web Mercator bounds')
    if config.get('profile', 'native-experimental') != 'native-experimental':
        raise ValueError('Only native-experimental is supported; full/matched OMT parity cannot be asserted')
    if 'compile_region' in Path(config['native_binary']).stem or 'compile-region' in Path(config['native_binary']).stem:
        raise ValueError('native_binary is the Tactical entry point; supply the compile_omt executable')


def summarize(runs, expected):
    report = common.summarize(runs, expected)
    report.update(reason=REASON, profile='native-experimental', matched_work=False, parity_verified=False)
    if report['status'] != 'failed':
        report['status'] = 'experimental_profile_comparison'
    return report


def comparison_manifest(config, inputs, result, engine):
    archive = next((p for p in result['artifacts'] if Path(p['path']).name == 'basemap.pmtiles'), None)
    return {'input_sha256': inputs['input_pbf']['sha256'], 'artifact_sha256': archive['sha256'] if archive else None,
            'schema': 'OpenMapTiles3.16.0', 'profile': 'native-experimental' if engine == 'native' else 'full-omt',
            'profile_revision': REVISION, 'parity_verified': False,
            'settings': {'minzoom': 0, 'maxzoom': 14, 'detailzoom': 14, 'threads': config['threads'],
                         'output': 'pmtiles-mvt', 'annotations': 'none', 'bounds_policy': 'explicit',
                         'bounds': config['bounds'], 'tile_compression': result.get('tile_compression'),
                         'wikidata': False, 'languages': 'OMT3.16.0-default',
                         'auxiliary_inputs': {Path(p['path']).relative_to(Path(config['planetiler_sources'])).as_posix(): p['sha256'] for p in inputs['planetiler_sources']}}}


def run(config, output, dry_run=False, engine="both"):
    output = Path(output).resolve()
    if output.exists():
        raise ValueError('Output directory already exists; choose a new directory')
    config = dict(config)
    config.setdefault('repeats', 1)
    validate_config(config)
    inputs = common.preflight(config)
    for key in ['input_pbf', 'native_binary', 'java_binary', 'planetiler_jar', 'planetiler_sources']:
        config[key] = str(Path(config[key]).resolve())
    output.mkdir(parents=True)
    manifest = {'format_version': 1, 'profile': 'native-experimental', 'matched_work': False,
                'parity_verified': False, 'reason': REASON, 'engine_selection': engine, 'config': config, 'files': inputs,
                'profile_revision': REVISION, 'harness_script': common.fingerprint(__file__),
                'common_harness_script': common.fingerprint(Path(__file__).with_name('run.py')),
                'memory_counters_script': common.fingerprint(Path(__file__).with_name('memory_counters.py')),
                'harness_checkout': common.git_info(Path(__file__).resolve().parents[2]),
                'native_build_provenance': {'asserted_revision': config.get('native_build_revision'),
                                          'verification': 'Binary hash recorded; supplied revision is an assertion.'},
                'host': {'platform': platform.platform(), 'python': sys.version, 'psutil': common.psutil.__version__,
                         'logical_cpus': common.psutil.cpu_count(), 'memory_bytes': common.psutil.virtual_memory().total},
                'limitations': common.LIMITATIONS,
                'inherited_environment': {k: os.environ[k] for k in ['JAVA_TOOL_OPTIONS', '_JAVA_OPTIONS', 'JDK_JAVA_OPTIONS', 'RUST_LOG', 'CARGO_TARGET_DIR'] if k in os.environ},
                'plans': [commands(config, output / f'repeat-{i:02d}', engine) for i in range(1, config['repeats'] + 1)]}
    common.write_json(output / 'manifest.json', manifest)
    if dry_run:
        common.write_json(output / 'report.json', {'status': 'dry_run', 'matched_work': False, 'parity_verified': False, 'reason': REASON})
        return 0
    runs = []
    expected = sum(len(plan) for plan in manifest['plans'])
    common.write_json(output / 'report.json', summarize(runs, expected))
    for repeat, plan in enumerate(manifest['plans'], 1):
        for engine, command in (plan if repeat % 2 else list(reversed(plan))):
            directory = output / f'repeat-{repeat:02d}' / engine
            directory.mkdir(parents=True)
            try:
                common.verify_fingerprints(config, inputs)
            except (OSError, ValueError) as exc:
                report = summarize(runs, expected)
                report.update(status='failed', run_state='failed', provenance_error=str(exc))
                common.write_json(output / 'report.json', report)
                return 1
            env = dict(os.environ, RAYON_NUM_THREADS=str(config['threads']))
            try:
                result = common.measure(command, directory, env, directory, config['timeout_seconds'],
                                        config['sample_interval_seconds'], config['rss_limit_gib'] * 1024**3)
            except OSError as exc:
                result = {'command': command, 'cwd': str(directory), 'exit_code': None, 'stop_reason': 'launch_error',
                          'error': str(exc), 'wall_seconds': None, 'sampled_peak_tree_rss_bytes': None}
            result.update(engine=engine, repeat=repeat)
            try:
                common.verify_fingerprints(config, inputs)
            except (OSError, ValueError) as exc:
                result['provenance_error'] = str(exc)
            result['artifacts'] = [common.fingerprint(p) for p in sorted(directory.glob('*.pmtiles'))]
            required = ['basemap.pmtiles'] + (['compile-result.json'] if engine == 'native' else [])
            result['missing_artifacts'] = [p for p in required if not (directory / p).is_file()]
            try:
                if (directory / 'basemap.pmtiles').is_file():
                    result['tile_compression'] = common.tile_compression(directory / 'basemap.pmtiles')
                if engine == 'native' and (directory / 'compile-result.json').is_file():
                    compiled = json.loads((directory / 'compile-result.json').read_text(encoding='utf-8-sig'))
                    result['compile_result'] = compiled
                    result['compile_result_fingerprint'] = common.fingerprint(directory / 'compile-result.json')
                    if compiled.get('profile_revision') != REVISION or compiled.get('parity_verified') is not False or not str(compiled.get('schema', '')).startswith('OpenMapTiles 3.16.0'):
                        raise ValueError('Native result is not the pinned experimental OpenMapTiles profile')
            except (OSError, ValueError) as exc:
                result['artifact_error'] = str(exc)
            result['success'] = result['exit_code'] == 0 and result['stop_reason'] is None and not result['missing_artifacts'] and not any(result.get(key) for key in ('cleanup_survivor_pids', 'provenance_error', 'artifact_error'))
            result['phase_trace'] = {'stdout': str(directory / 'stdout.log'), 'stderr': str(directory / 'stderr.log'),
                                     'events': common.phase_events(directory / 'stderr.log') if (directory / 'stderr.log').exists() else [],
                                     'clock_note': 'Native compile-result durations use internal clocks; no exact RSS alignment is claimed.'}
            common.write_json(directory / 'measurement.json', result)
            common.write_json(directory / 'comparison-manifest.json', comparison_manifest(config, inputs, result, engine))
            runs.append(result)
            common.write_json(output / 'report.json', summarize(runs, expected))
            if not result['success']:
                return 1
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=1, help='Explicit repeat count; defaults to one even if legacy config has more')
    parser.add_argument('--engine', choices=['both', 'native', 'planetiler'], default='both')
    parser.add_argument('--dry-run', action='store_true')
    args = parser.parse_args()
    try:
        config = json.loads(args.config.read_text(encoding='utf-8-sig'))
        config['repeats'] = args.repeats
        return run(config, args.output, args.dry_run, args.engine)
    except (OSError, ValueError, KeyError) as exc:
        print(str(exc), file=sys.stderr)
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
