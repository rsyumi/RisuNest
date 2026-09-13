"""Drive only the isolated agent app, using real LaunchServices open/reopen events."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def records(path):
    # A report can be large; the reader may observe an unfinished final write.
    return [json.loads(line) for line in path.read_text().splitlines(keepends=True) if line.endswith('\n')] if path.exists() else []


def memory_sample(pid):
    # RSS is a process-residency observation, not physical footprint or private memory.
    output = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,rss=,comm='], text=True)
    rows = []
    for line in output.splitlines():
        fields = line.split(None, 3)
        if len(fields) == 4:
            rows.append((int(fields[0]), int(fields[1]), int(fields[2]), fields[3]))
    selected = {pid}
    for _ in range(8):
        selected.update(row[0] for row in rows if row[1] in selected)
    # WKWebView services can be launched by launchd. Keep a separate un-attributed
    # system-wide observation instead of claiming those all belong to this app.
    own = [{'pid': p, 'rssKiB': rss} for p, _, rss, _ in rows if p in selected]
    webkit = [{'pid': p, 'rssKiB': rss} for p, _, rss, name in rows if 'WebKit' in name]
    return {'time': time.time(), 'appTree': own, 'systemWebKit': webkit}


def run_phase(app, phase, artifacts, fixtures):
    report = artifacts / f'{phase}.jsonl'
    environment = {**os.environ, 'RISUNEST_MACOS_PHASE': phase, 'RISUNEST_MACOS_REPORT': str(report)}
    binary = app / 'Contents/MacOS/risunest-macos-bench'
    samples = []
    handled = set()
    (artifacts / f'{phase}-baseline-memory.json').write_text(json.dumps(memory_sample(-1), indent=2))
    with (artifacts / f'{phase}.log').open('w') as log:
        process = subprocess.Popen([str(binary)], env=environment, stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 600
            while process.poll() is None:
                for record in records(report):
                    stage = record['stage']
                    if stage == 'failure':
                        raise RuntimeError(json.dumps(record))
                    if stage in handled:
                        continue
                    handled.add(stage)
                    print(f'{phase}: {stage}', flush=True)
                    if stage == 'closed':
                        subprocess.run(['open', '-a', str(app)], check=True)
                    if stage == 'reopened':
                        subprocess.run(['open', '-a', str(app), *map(str, fixtures)], check=True)
                    if stage == 'app':
                        subprocess.run(['screencapture', '-x', str(artifacts / 'app.png')], check=False)
                samples.append(memory_sample(process.pid))
                if time.monotonic() > deadline:
                    raise TimeoutError(f'{phase}: native app timeout, stages={sorted(handled)}')
                time.sleep(0.5)
            if process.returncode != 0:
                raise RuntimeError(f'{phase}: app exited {process.returncode}')
            result = records(report)
            required = {'contracts': {'persistence', 'regex', 'tokenizer', 'reload', 'closed', 'reopened', 'finder', 'quit-cancelled', 'quit-saved'}, 'restart': {'restart'}, 'app': {'app'}, 'app-restart': {'app-restart'}}[phase]
            stages = {entry['stage'] for entry in result}
            if not required <= stages or 'failure' in stages:
                raise RuntimeError(f'{phase}: incomplete results {stages}')
            return result
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)
            (artifacts / f'{phase}-memory.json').write_text(json.dumps(samples, indent=2))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--artifacts', type=Path, required=True)
    args = parser.parse_args()
    app = args.app.resolve(strict=True)
    identifier = subprocess.check_output(['/usr/libexec/PlistBuddy', '-c', 'Print :CFBundleIdentifier', str(app / 'Contents/Info.plist')], text=True).strip()
    if identifier != 'io.github.rsyumi.risunest.macos.bench':
        raise RuntimeError('Refusing a non-harness app')
    profile = Path.home() / 'Library/Application Support' / identifier
    if profile.exists():
        raise RuntimeError('Harness requires a fresh CI user profile; refusing existing data')
    artifacts = args.artifacts.resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    fixture_root = Path(tempfile.mkdtemp(prefix='risunest-macos-', dir='/private/tmp'))
    fixtures = [fixture_root / 'synthetic 한글 # %.risup', fixture_root / 'synthetic-two.risum']
    for fixture in fixtures:
        fixture.write_text('synthetic file association fixture')
    results = {phase: run_phase(app, phase, artifacts, fixtures) for phase in ['contracts', 'restart', 'app', 'app-restart']}
    (artifacts / 'result.json').write_text(json.dumps({'passed': True, 'phases': results}, indent=2))
    print('Mac WKWebView contracts, restart and product app passed', flush=True)


if __name__ == '__main__':
    main()
