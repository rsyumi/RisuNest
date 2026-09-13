"""External WebKitWebDriver runner. Only starts an isolated synthetic benchmark build."""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request
import urllib.error

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--phase', choices=['persistence', 'reload', 'regex'], required=True)
args = parser.parse_args()
out = Path(args.output).resolve()
out.mkdir(parents=True, exist_ok=True)
marker = out / 'synthetic-profile.txt'
if marker.exists():
    profile = Path(marker.read_text().strip())
    if profile.parent != Path('/var/tmp') or not profile.name.startswith('risunest-linux-bench-') or profile.stat().st_uid != os.getuid():
        raise RuntimeError('Invalid synthetic profile')
else:
    profile = Path(tempfile.mkdtemp(prefix='risunest-linux-bench-', dir='/var/tmp'))
    marker.write_text(str(profile))
for key, leaf in [('XDG_DATA_HOME', 'data'), ('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache')]:
    location = profile / leaf
    location.mkdir(exist_ok=True)
    os.environ[key] = str(location)
os.environ['TAURI_WEBVIEW_AUTOMATION'] = 'true'
os.environ['GDK_BACKEND'] = 'x11'
with socket.socket() as probe:
    probe.bind(('127.0.0.1', 0))
    port = probe.getsockname()[1]
base = f'http://127.0.0.1:{port}'
log = (out / f'{args.phase}-driver.log').open('w')
driver = subprocess.Popen(['WebKitWebDriver', '--host=127.0.0.1', f'--port={port}'], stdout=log, stderr=log)
session = None
stop = threading.Event()
memory = []

def call(method, route, payload=None):
    body = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(base + route, data=body, method=method, headers={'Content-Type': 'application/json'})
    try:
        with urllib.request.urlopen(req, timeout=60) as response:
            return json.load(response)['value']
    except urllib.error.HTTPError as error:
        raise RuntimeError(error.read().decode()) from error

def execute(script):
    return call('POST', f'/session/{session}/execute/sync', {'script': script, 'args': []})

def descendants(pid):
    try:
        children = set()
        for task in Path(f'/proc/{pid}/task').iterdir():
            try:
                children.update((task / 'children').read_text().split())
            except OSError:
                continue
    except OSError:
        return []
    result = []
    for child in children:
        result.append(int(child))
        result.extend(descendants(child))
    return result

def monitor():
    # Linux PSS avoids double-counting shared pages across WebKit subprocesses.
    while not stop.wait(0.2):
        totals = {'pssKiB': 0, 'ussKiB': 0, 'processes': 0}
        for pid in descendants(driver.pid):
            try:
                rows = Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines()
                values = {row.split(':')[0]: int(row.split()[1]) for row in rows if row.split()[0].endswith(':')}
                totals['pssKiB'] += values.get('Pss', 0)
                totals['ussKiB'] += sum(values.get(key, 0) for key in ['Private_Clean', 'Private_Dirty', 'Private_Hugetlb'])
                totals['processes'] += 1
            except (OSError, ValueError):
                continue
        memory.append(totals)

try:
    for attempt in range(100):
        try:
            call('GET', '/status')
            break
        except OSError:
            time.sleep(0.1)
    created = call('POST', '/session', {'capabilities': {'alwaysMatch': {'webkitgtk:browserOptions': {'binary': str(Path(args.binary).resolve()), 'args': []}}}})
    session = created['sessionId']
    call('POST', f'/session/{session}/window/rect', {'width': 1280, 'height': 900})
    for attempt in range(100):
        if execute('return !!window.__TAURI_INTERNALS__'):
            break
        time.sleep(0.1)
    identity = call('POST', f'/session/{session}/execute/async', {'script': '''const done=arguments[arguments.length-1]; Promise.all([window.__TAURI_INTERNALS__.invoke('plugin:app|identifier'),window.__TAURI_INTERNALS__.invoke('plugin:path|resolve_directory',{directory:14})]).then(done,()=>done(null));''', 'args': []})
    if not identity or identity[0] != 'io.github.rsyumi.risunest.linux.bench' or Path(identity[1]).resolve() != (profile / 'data' / identity[0]).resolve():
        raise RuntimeError('Synthetic identity/path gate failed')
    for attempt in range(100):
        if execute('return !!window.__RISUNEST_LINUX_BENCHMARK__'):
            break
        time.sleep(0.1)
    thread = threading.Thread(target=monitor, daemon=True)
    thread.start()
    execute(f"window.__linuxResult=null; window.__RISUNEST_LINUX_BENCHMARK__.{args.phase}().then(result=>window.__linuxResult={{result}},error=>window.__linuxResult={{error:String(error)}})")
    deadline = time.monotonic() + 600
    while time.monotonic() < deadline:
        result = execute('return window.__linuxResult')
        if result is not None:
            break
        time.sleep(0.5)
    else:
        raise RuntimeError('Benchmark timed out')
    if 'error' in result:
        raise RuntimeError(result['error'])
    result = result['result']
    stop.set()
    thread.join(timeout=2)
    if not memory or not any(item['processes'] for item in memory):
        raise RuntimeError('No process memory samples collected')
    result['memorySamples'] = memory
    result['environment'] = {'display': 'Xvfb/X11', 'backend': 'WebKitGTK', 'storage': 'synthetic /var/tmp', 'memoryScope': 'WebDriver descendants; sampled every 200ms, not per-mode attribution'}
    if args.phase == 'reload':
        previous = json.loads((out / 'persistence.json').read_text())
        if result['revision'] != previous['revision'] or result['finalHash'] != previous['finalHash']:
            raise RuntimeError('Restart hash/revision mismatch')
    (out / f'{args.phase}.json').write_text(json.dumps(result, indent=2))
    print(json.dumps({'phase': args.phase, 'passed': True, 'samples': len(result.get('samples', [])), 'memorySamples': len(memory)}))
finally:
    stop.set()
    if session:
        try:
            call('DELETE', f'/session/{session}')
        except Exception:
            pass
    driver.terminate()
    try:
        driver.wait(timeout=10)
    except subprocess.TimeoutExpired:
        driver.kill()
        driver.wait()
    log.close()
