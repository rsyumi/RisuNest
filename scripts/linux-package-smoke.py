"""Run an agent package in a private XDG profile and verify native persistence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request
import urllib.error

parser = argparse.ArgumentParser()
parser.add_argument('--binary', required=True)
parser.add_argument('--output', required=True)
parser.add_argument('--phase', choices=['first', 'reinstall', 'appimage'], required=True)
args = parser.parse_args()
out = Path(args.output).resolve()
out.mkdir(parents=True, exist_ok=True)
marker = out / 'synthetic-profile.txt'
if marker.exists():
    profile = Path(marker.read_text().strip())
    if profile.parent != Path('/var/tmp') or not profile.name.startswith('risunest-package-') or profile.stat().st_uid != os.getuid():
        raise RuntimeError('Invalid synthetic profile')
else:
    if args.phase != 'first':
        raise RuntimeError('Run first phase before upgrade or AppImage checks')
    profile = Path(tempfile.mkdtemp(prefix='risunest-package-', dir='/var/tmp'))
    marker.write_text(str(profile))
for key, leaf in [('XDG_DATA_HOME', 'data'), ('XDG_CONFIG_HOME', 'config'), ('XDG_CACHE_HOME', 'cache')]:
    directory = profile / leaf
    directory.mkdir(exist_ok=True)
    os.environ[key] = str(directory)
os.environ['TAURI_WEBVIEW_AUTOMATION'] = 'true'
os.environ['GDK_BACKEND'] = 'x11'
with socket.socket() as probe:
    probe.bind(('127.0.0.1', 0))
    port = probe.getsockname()[1]
base = f'http://127.0.0.1:{port}'
log = (out / f'{args.phase}-driver.log').open('w')
driver = subprocess.Popen(['WebKitWebDriver', '--host=127.0.0.1', f'--port={port}'], stdout=log, stderr=log)
session = None

def call(method, route, payload=None):
    request = urllib.request.Request(base + route, data=None if payload is None else json.dumps(payload).encode(), method=method, headers={'Content-Type':'application/json'})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)['value']
    except urllib.error.HTTPError as error:
        raise RuntimeError(error.read().decode()) from error

def execute(script):
    return call('POST', f'/session/{session}/execute/sync', {'script':script, 'args':[]})

def invoke(command, payload=None):
    return call('POST', f'/session/{session}/execute/async', {'script':'''const done=arguments[arguments.length-1];window.__TAURI_INTERNALS__.invoke(arguments[0],arguments[1]).then(value=>done({value}),error=>done({error:String(error)}));''','args':[command,payload or {}]})

def native(command, payload=None):
    result = invoke(command, payload)
    if 'error' in result:
        raise RuntimeError(result['error'])
    return result.get('value')

def wait(script):
    for attempt in range(160):
        if execute(script):
            return
        time.sleep(0.25)
    raise RuntimeError('Synthetic UI condition timed out')

try:
    for attempt in range(100):
        try:
            call('GET', '/status')
            break
        except OSError:
            time.sleep(0.1)
    uri = 'risunestlocal://packaging-smoke'
    session = call('POST', '/session', {'capabilities':{'alwaysMatch':{'webkitgtk:browserOptions':{'binary':str(Path(args.binary).resolve()),'args':[uri]}}}})['sessionId']
    wait('return !!window.__TAURI_INTERNALS__')
    identifier = native('plugin:app|identifier')
    app_data = native('plugin:path|resolve_directory', {'directory':14})
    if identifier != 'io.github.rsyumi.risunest' or Path(app_data).resolve() != (profile / 'data' / identifier).resolve():
        raise RuntimeError('Native identity/path gate failed; no DOM or data inspected')
    current = native('plugin:deep-link|get_current')
    if current != [uri]:
        raise RuntimeError('Cold deep link was not delivered')
    if args.phase == 'first':
        wait('return !!document.querySelector(".panel-in button.row.primary")')
        execute('document.querySelector(".panel-in button.row.primary").click()')
        wait('return !!document.querySelector(".done button.primary")')
        execute('document.querySelector(".done button.primary").click()')
        wait('return !document.querySelector(".panel-in") && !document.body.innerText.startsWith("Loading")')
        time.sleep(1)
        execute('Array.from(document.querySelectorAll("button")).find(x=>x.innerText.trim()==="Accept")?.click()')
        time.sleep(1)
        execute('document.querySelector(\'path[d="M12 6v6m0 0v6m0-6h6m-6 0H6"]\').closest("button").click()')
        wait('return Array.from(document.querySelectorAll("button")).some(x=>x.innerText.trim()==="Create from Scratch")')
        execute('Array.from(document.querySelectorAll("button")).find(x=>x.innerText.trim()==="Create from Scratch").click()')
        wait('return Array.from(document.querySelectorAll("button")).some(x=>x.innerText.trim()==="New Chat")')
        time.sleep(1)
        native('pds_set_app_kv', {'key':'linux-package-smoke','value':{'marker':'synthetic 한글 🐿️','version':1}})
    else:
        wait('return !document.body.innerText.startsWith("Loading")')
        time.sleep(1)
    catalog = native('pds_query_characters', {'query':{'order':'configured','trash':False,'limit':100}})
    if len(catalog['items']) != 1 or catalog['items'][0]['name'] != '' or catalog['items'][0]['conversationCount'] != 1:
        raise RuntimeError('Created character/conversation was not preserved')
    sentinel = native('pds_get_app_kv', {'key':'linux-package-smoke'})
    if sentinel != {'marker':'synthetic 한글 🐿️','version':1}:
        raise RuntimeError('Synthetic persistent key/value was not preserved')
    digest = hashlib.sha256(json.dumps({'items':catalog['items'],'sentinel':sentinel}, sort_keys=True).encode()).hexdigest()
    result = {'passed':True, 'phase':args.phase, 'identifier':identifier, 'coldDeepLink':True, 'catalogHash':digest, 'characters':1, 'conversations':1}
    if args.phase != 'first':
        first = json.loads((out / 'first.json').read_text())
        if first['catalogHash'] != digest:
            raise RuntimeError('Package reinstall or format switch changed synthetic data')
    # Same-instance protocol delivery must update the native plugin's current URI.
    warm_uri = 'risunestlocal://packaging-smoke-warm'
    subprocess.run([str(Path(args.binary).resolve()), warm_uri], check=True, timeout=30, stdout=log, stderr=log)
    for attempt in range(40):
        if native('plugin:deep-link|get_current') == [warm_uri]:
            result['warmDeepLink'] = True
            break
        time.sleep(0.1)
    else:
        raise RuntimeError('Warm deep link was not delivered')
    if args.phase != 'appimage':
        desktop_uri = 'risunestlocal://packaging-desktop'
        subprocess.run(['gio', 'launch', '/usr/share/applications/RisuNest.desktop', desktop_uri], check=True, timeout=30, stdout=log, stderr=log)
        for attempt in range(40):
            if native('plugin:deep-link|get_current') == [desktop_uri]:
                result['desktopDeepLink'] = True
                break
            time.sleep(0.1)
        else:
            raise RuntimeError('Desktop launcher lost the URL argument')
    callback = execute('window.__packageOpenedFiles=null;return window.__TAURI_INTERNALS__.transformCallback(e=>window.__packageOpenedFiles=e.payload)')
    native('plugin:event|listen', {'event':'risu-opened-files','target':{'kind':'Any'},'handler':callback})
    fixture = profile / 'synthetic 한글 # %.charx'
    fixture.write_bytes(b'synthetic invalid card for delivery-only check')
    launch = [str(Path(args.binary).resolve())] if args.phase == 'appimage' else ['gio', 'launch', '/usr/share/applications/RisuNest.desktop']
    subprocess.run(launch + [fixture.as_uri()], check=True, timeout=30, stdout=log, stderr=log)
    wait('return !!window.__packageOpenedFiles')
    if execute('return window.__packageOpenedFiles.files') != [str(fixture.resolve())]:
        raise RuntimeError('File URL was not delivered as the exact canonical path')
    result['fileUrlDelivery'] = True
    (out / f'{args.phase}.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result))
finally:
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
