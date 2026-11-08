"""Drive only the isolated agent app, using real LaunchServices open/reopen events."""
import argparse
import json
import os
import re
import shutil
import sys
from pathlib import Path
import subprocess
import tempfile
import time

from sync_controller import DEPENDENCIES as SYNC_DEPENDENCIES, PHASES as SYNC_PHASES, SyncEnvironmentError, SyncSession
from external_controller import (DEPENDENCIES as EXTERNAL_DEPENDENCIES, FILE_PHASES, GROUPS as EXTERNAL_GROUPS,
                                 ExternalStorageSession, file_markers)

EXTERNAL_PHASES = tuple(phase for group in EXTERNAL_GROUPS for phase in group)


def records(path):
    # A report can be large; the reader may observe an unfinished final write.
    return [json.loads(line) for line in path.read_text().splitlines(keepends=True) if line.endswith('\n')] if path.exists() else []


def activate_owned_process(process, binary):
    # A directly launched bundle executable does not imply foreground activation.
    script = r"""
ObjC.import('AppKit');
function run(argv) {
    var pid = Number(argv[0]);
    var expected = argv[1];
    var deadline = Date.now() + 10000;
    var last = { pid: pid, frontmostPid: null, found: false, activationRequested: false };
    while (Date.now() < deadline) {
        var applications = $.NSWorkspace.sharedWorkspace.runningApplications;
        var target = null;
        last.frontmostPid = null;
        for (var i = 0; i < applications.count; i++) {
            var candidate = applications.objectAtIndex(i);
            if (candidate.isActive) last.frontmostPid = Number(candidate.processIdentifier);
            if (Number(candidate.processIdentifier) === pid) target = candidate;
        }
        if (target !== null) {
            last.found = true;
            var executable = ObjC.unwrap(target.executableURL.path.stringByResolvingSymlinksInPath);
            if (executable !== expected) throw new Error('Owned process executable mismatch');
            if (last.frontmostPid === pid) return JSON.stringify(last);
            last.activationRequested = true;
            target.activateWithOptions($.NSApplicationActivateAllWindows | $.NSApplicationActivateIgnoringOtherApps);
        } else if (last.found) {
            throw new Error('Owned application exited before foreground confirmation');
        }
        $.NSRunLoop.currentRunLoop.runUntilDate($.NSDate.dateWithTimeIntervalSinceNow(0.05));
    }
    throw new Error('Owned application did not become foreground: ' + JSON.stringify(last));
}
"""
    result = subprocess.run(['osascript', '-l', 'JavaScript', '-e', script, str(process.pid), str(binary.resolve())],
                            capture_output=True, text=True, timeout=15)
    if result.returncode != 0:
        if process.poll() is not None:
            # Fast phases may finish before the external observer sees their activation.
            # Their existing exit and complete-result checks still decide success.
            return {'pid': process.pid, 'confirmed': False, 'exited': process.returncode}
        raise RuntimeError(f'Owned app activation failed: {result.stderr.strip()}')
    observation = json.loads(result.stdout)
    if observation.get('pid') != process.pid or observation.get('frontmostPid') != process.pid:
        raise RuntimeError('Foreground confirmation did not identify the owned process')
    return {**observation, 'confirmed': True}


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


def start_appearance_capture(directory, label, platform, capture_input, capture_size=None, display_owner_pid=None, experiment_codec=None, bgra_experiment=False):
    if experiment_codec is not None and (platform not in {'macos', 'linux'} or experiment_codec not in {'ffv1', 'copy'}):
        raise RuntimeError('Paired capture experiment supports only Mac/Linux ffv1 and copy controls')
    if bgra_experiment and (platform != 'macos' or experiment_codec != 'copy'):
        raise RuntimeError('BGRA capture experiment supports only Mac stream-copy')
    if not shutil.which('ffmpeg') or not shutil.which('ffprobe'):
        raise RuntimeError('Appearance capture requires ffmpeg and ffprobe')
    if platform == 'macos':
        if os.environ.get('GITHUB_ACTIONS') != 'true':
            raise RuntimeError('Mac whole-display capture is restricted to the fresh hosted CI desktop')
        if not re.fullmatch(r'Capture screen [0-9]+:none', capture_input or ''):
            raise RuntimeError('Select an enumerated Capture screen device without audio')
        pixel_format = ['-pixel_format', 'bgr0'] if bgra_experiment else []
        source = ['-f', 'avfoundation', '-framerate', '60', *pixel_format, '-i', capture_input]
        desktop = {'kind': 'hosted macOS desktop'}
    else:
        display = os.environ.get('DISPLAY', '')
        if not display or capture_input != display or not display_owner_pid:
            raise RuntimeError('Linux capture needs DISPLAY and its owned Xvfb PID')
        process_root = Path('/proc') / str(display_owner_pid)
        command = (process_root / 'cmdline').read_bytes().split(b'\0')
        if (process_root.stat().st_uid != os.getuid() or not command
                or Path(os.fsdecode(command[0])).name != 'Xvfb'
                or display.split('.')[0].encode() not in command):
            raise RuntimeError('Capture display is not the owned synthetic Xvfb process')
        wm = subprocess.check_output(['xprop', '-root', '_NET_SUPPORTING_WM_CHECK'], text=True)
        match = re.search(r'0x[0-9a-fA-F]+', wm)
        if not match or int(match.group(), 16) == 0:
            raise RuntimeError('A named window manager is required for title-bar observations')
        wm_name = subprocess.check_output(['xprop', '-id', match.group(), '_NET_WM_NAME'], text=True).strip()
        if ' = ' not in wm_name:
            raise RuntimeError('Window manager name unavailable')
        if not re.fullmatch(r'[1-9][0-9]*x[1-9][0-9]*', capture_size or ''):
            raise RuntimeError('Explicit capture size required')
        source = ['-f', 'x11grab', '-framerate', '60', '-video_size', capture_size, '-i', display]
        desktop = {'kind': 'owned Xvfb/X11', 'windowManager': wm_name, 'gtkTheme': os.environ.get('GTK_THEME')}
    video = directory / f'{label}.{"nut" if experiment_codec else "mkv"}'
    progress = directory / f'{label}-capture-progress.txt'
    if video.exists() or progress.exists():
        raise RuntimeError('Refusing to overwrite appearance capture')
    timestamp_options = ['-debug_ts'] if platform == 'linux' and experiment_codec else []
    ffmpeg_version = (subprocess.check_output(['ffmpeg', '-version'], text=True).splitlines()[0]
                      if timestamp_options else None)
    log = (directory / f'{label}-capture.log').open('w')
    started = time.monotonic()
    experiment_options = ['-nostdin', '-benchmark', '-t', '6', '-f', 'nut'] if experiment_codec else []
    process = subprocess.Popen(['ffmpeg', '-hide_banner', '-loglevel', 'info' if experiment_codec else 'warning', '-n', *timestamp_options, '-thread_queue_size', '32', *source,
        '-an', '-c:v', experiment_codec or 'ffv1', '-fps_mode', 'passthrough', '-progress', str(progress),
        *experiment_options, str(video)],
        stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT)
    try:
        deadline = started + 20
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError('Screen capture exited before application launch')
            frames = re.findall(r'^frame=(\d+)$', progress.read_text() if progress.exists() else '', re.M)
            if frames and int(frames[-1]) >= 2:
                return {'process': process, 'log': log, 'video': video, 'started': started,
                        'launchOffsetSeconds': time.monotonic() - started, 'desktop': desktop,
                        'experimentCodec': experiment_codec, 'sourceTimestampDiagnostics': bool(timestamp_options),
                        'ffmpegVersion': ffmpeg_version, 'progress': progress, 'logPath': Path(log.name)}
            time.sleep(0.05)
        raise RuntimeError('No prelaunch frames received from screen capture')
    except BaseException:
        process.terminate()
        process.wait(timeout=10)
        log.close()
        raise


def finish_appearance_capture(capture, require_display_cadence=True):
    process = capture['process']
    try:
        process.communicate(None if capture.get('experimentCodec') else b'q\n', timeout=15)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
        raise RuntimeError('Screen capture did not stop')
    finally:
        capture['log'].close()
    if process.returncode != 0:
        raise RuntimeError('Screen capture failed')
    probe = json.loads(subprocess.check_output(['ffprobe', '-v', 'error', '-select_streams', 'v:0',
        '-show_frames', '-show_entries', 'frame=best_effort_timestamp_time', '-of', 'json', str(capture['video'])], text=True))
    times = [float(frame['best_effort_timestamp_time']) for frame in probe['frames']]
    if capture.get('experimentCodec'):
        capture['video'].with_suffix('.frame-timestamps.json').write_text(json.dumps(probe, indent=2))
    gaps = [end - start for start, end in zip(times, times[1:])]
    result = {'requestedFps': 60, 'frames': len(times), 'maximumFrameGapSeconds': max(gaps, default=None),
              'launchOffsetSeconds': capture['launchOffsetSeconds'], 'desktop': capture['desktop'],
              'visualReview': 'required', 'contentBackgroundPass': None, 'titlebarPass': None,
              'scope': 'cold process launch, filesystem caches not reset'}
    if capture.get('sourceTimestampDiagnostics'):
        text = capture['logPath'].read_text()
        input_video = re.search(r'Video: rawvideo[^\r\n]*', text)
        statistics = re.search(r'bench: utime=([\d.]+)s stime=([\d.]+)s rtime=([\d.]+)s', text)
        progress = capture['progress'].read_text()
        duplicates = re.findall(r'^dup_frames=(\d+)$', progress, re.M)
        drops = re.findall(r'^drop_frames=(\d+)$', progress, re.M)
        result['diagnosticOnly'] = True
        result['experimentCodec'] = capture['experimentCodec']
        result['ffmpegVersion'] = capture['ffmpegVersion']
        result['inputVideo'] = input_video.group() if input_video else None
        result['benchmarkSeconds'] = (dict(zip(['user', 'system', 'elapsed'], map(float, statistics.groups())))
                                      if statistics else None)
        result['duplicateFrames'] = int(duplicates[-1]) if duplicates else None
        result['droppedFrames'] = int(drops[-1]) if drops else None
        result['timestampProvenance'] = {
            'source': 'input demuxer packet timestamps in FFmpeg -debug_ts log',
            'sourceLog': capture['logPath'].name,
            'output': 'ffprobe decoded frame best_effort_timestamp_time',
            'outputFrames': capture['video'].with_suffix('.frame-timestamps.json').name}
    capture['video'].with_suffix('.json').write_text(json.dumps(result, indent=2))
    if capture.get('sourceTimestampDiagnostics'):
        if not result['inputVideo'] or result['benchmarkSeconds'] is None:
            raise RuntimeError('Capture input format or CPU timing unavailable')
        if result['duplicateFrames'] != 0 or result['droppedFrames'] != 0:
            raise RuntimeError('Capture output duplicated or dropped frames')
    if not times:
        raise RuntimeError('Screen capture recorded no frames')
    if require_display_cadence and (len(times) < 60 or not gaps or max(gaps) > 0.04):
        raise RuntimeError('Capture cadence insufficient to assess a one-frame startup flash')
    return result


def run_phase(app, phase, artifacts, fixtures, expected=None):
    report = artifacts / f'{phase}.jsonl'
    environment = {**os.environ, 'RISUNEST_MACOS_PHASE': phase, 'RISUNEST_MACOS_REPORT': str(report)}
    if expected is not None:
        environment['RISUNEST_MACOS_EXPECTED'] = json.dumps(expected)
    binary = app / 'Contents/MacOS/risunest-macos-bench'
    samples = []
    handled = set()
    (artifacts / f'{phase}-baseline-memory.json').write_text(json.dumps(memory_sample(-1), indent=2))
    with (artifacts / f'{phase}.log').open('w') as log:
        process = subprocess.Popen([str(binary)], env=environment, stdout=log, stderr=subprocess.STDOUT)
        try:
            activation = activate_owned_process(process, binary)
            (artifacts / f'{phase}-activation.json').write_text(json.dumps(activation, indent=2))
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
            required = ({'session-dispatch-ready', 'session-dispatch-sent', 'session-dispatch-exit'}
                        if phase.startswith('session-dispatch-') else
                        {phase} if phase.startswith('appearance-') else
                        {'session-upgrade': {'session-deadline-started', 'session-deadline-reply', 'session-deadline-exit'}, 'session-deadline': {'session-deadline-started', 'session-deadline-reply', 'session-deadline-exit'}, 'termination-probe': {'termination-probe-cancel', 'termination-probe-reload', 'termination-probe-approved'}, 'contracts': {'persistence', 'regex', 'tokenizer', 'reload', 'closed', 'reopened', 'finder', 'quit-cancelled', 'quit-saved'}, 'restart': {'restart'}, 'app': {'app', 'app-native-saving', 'app-native-reload-cancelled', 'app-native-stale-rejected', 'app-native-saved', 'app-native-exit'}, 'app-restart': {'app-restart'}, 'streaming': {'streaming'}, 'quit-escape': {'quit-escape-delivered', 'quit-escape-exit'},
                         **{name: {name} for name in (*SYNC_PHASES, *EXTERNAL_PHASES)}}[phase])
            stages = {entry['stage'] for entry in result}
            if not required <= stages or 'failure' in stages:
                raise RuntimeError(f'{phase}: incomplete results {stages}')
            if phase.startswith('session-dispatch-'):
                from termination_dispatch import validate_dispatch
                observation = validate_dispatch(phase, result)
                (artifacts / f'{phase}-observation.json').write_text(json.dumps(observation, indent=2))
            if phase.startswith('appearance-'):
                appearance = [entry['result'] for entry in result if entry['stage'] == phase]
                theme = phase.rsplit('-', 1)[1]
                if len(appearance) != 1 or not isinstance(appearance[0], dict) or (
                    appearance[0].get('theme') != theme
                    or appearance[0].get('nativeTheme') != theme
                    or appearance[0].get('nativeThemeScope') != 'app'
                    or appearance[0].get('nativeThemeMatchesApp') is not True
                    or appearance[0].get('colorScheme') != theme
                ):
                    raise RuntimeError(f'{phase}: matching public native app theme readback required')
            if phase == 'app':
                replies = [(index, entry['result']) for index, entry in enumerate(result) if entry['stage'] == 'app-native-reply']
                saving = [(index, entry['result']) for index, entry in enumerate(result) if entry['stage'] == 'app-native-saving']
                if len(replies) != 2 or replies[0][1]['approve'] is not False or replies[1][1]['approve'] is not True:
                    raise RuntimeError('Product native quit requires exactly NO then YES')
                if [reply['replyCount'] for _, reply in replies] != [1, 2] or any(
                    reply['mainThread'] is not True or reply['modal'] is not True or reply['runtimeQuitRequests'] != 0
                    for _, reply in replies
                ):
                    raise RuntimeError('Product native replies must run on the modal AppKit thread without runtime quits')
                if len(saving) != 2 or [entry['attempt'] for _, entry in saving] != [1, 2] or any(
                    entry['passed'] is not True or entry['nativeRequests'] != 1 or entry['flushCalls'] != 1
                    for _, entry in saving
                ):
                    raise RuntimeError('Both native product requests must reach one real saving UI and flush')
                settled = {}
                for stage in ['app-native-reload-cancelled', 'app-native-stale-rejected', 'app-native-saved', 'app-native-exit']:
                    entries = [(index, entry['result']) for index, entry in enumerate(result) if entry['stage'] == stage]
                    if len(entries) != 1 or entries[0][1]['passed'] is not True:
                        raise RuntimeError(f'Product native quit requires one passing {stage}')
                    settled[stage] = entries[0]
                if settled['app-native-reload-cancelled'][1]['replyCount'] != 1 or settled['app-native-stale-rejected'][1]['replyCount'] != 1:
                    raise RuntimeError('Reload and stale response must retain exactly one native reply')
                saved_revision = settled['app-native-saved'][1]['revision']
                exited = settled['app-native-exit'][1]
                if type(saved_revision) is not int or saved_revision < 0 or exited['nativeReplies'] != 2 or exited['runtimeQuitRequests'] != 0 or exited['exitCount'] != 1 or exited['productHandlerReturned'] is not True:
                    raise RuntimeError('Product native saved revision and forwarded Exit evidence required')
                order = [saving[0][0], replies[0][0], settled['app-native-reload-cancelled'][0],
                         settled['app-native-stale-rejected'][0], saving[1][0], settled['app-native-saved'][0],
                         replies[1][0], settled['app-native-exit'][0]]
                if order != sorted(set(order)):
                    raise RuntimeError('Product native cancellation, save, approval and Exit occurred out of order')
            if phase == 'app-restart':
                restarted = [entry['result'] for entry in result if entry['stage'] == 'app-restart']
                if len(restarted) != 1 or restarted[0]['passed'] is not True or type(restarted[0]['revision']) is not int or restarted[0]['revision'] < 0:
                    raise RuntimeError('Product restart requires one exact saved revision readback')
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
    parser.add_argument('--appearance', action='store_true')
    parser.add_argument('--app-lock', action='store_true')
    parser.add_argument('--capture-codec-experiment', action='store_true')
    parser.add_argument('--capture-bgra-experiment', action='store_true')
    parser.add_argument('--capture-input')
    parser.add_argument('--system-theme', choices=['light', 'dark'])
    parser.add_argument('--sync-server', type=Path)
    parser.add_argument('--webdav-server', type=Path)
    parser.add_argument('--webdav-scheme', choices=['https', 'http'], default='https')
    args = parser.parse_args()
    if args.app_lock and (args.appearance or args.capture_codec_experiment or args.capture_bgra_experiment):
        parser.error('Select one native frame capture mode')
    if args.appearance and args.capture_codec_experiment:
        parser.error('Select appearance capture or the paired codec experiment')
    if args.capture_codec_experiment and args.system_theme != 'light':
        parser.error('Paired capture experiment requires --system-theme light')
    if args.capture_bgra_experiment and (args.appearance or args.capture_codec_experiment):
        parser.error('Select BGRA capture experiment without other capture modes')
    if args.capture_bgra_experiment and args.system_theme != 'light':
        parser.error('BGRA capture experiment requires --system-theme light')
    app = args.app.resolve(strict=True)
    identifier = subprocess.check_output(['/usr/libexec/PlistBuddy', '-c', 'Print :CFBundleIdentifier', str(app / 'Contents/Info.plist')], text=True).strip()
    if identifier != 'io.github.rsyumi.risunest.macos.bench':
        raise RuntimeError('Refusing a non-harness app')
    profile = Path.home() / 'Library/Application Support' / identifier
    if profile.exists():
        raise RuntimeError('Harness requires a fresh CI user profile; refusing existing data')
    artifacts = args.artifacts.resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    if args.app_lock:
        if any(artifacts.iterdir()):
            raise RuntimeError('App-lock capture requires an empty artifact directory')
        sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'appLock'))
        from frames import run_macos
        run_macos(app, artifacts, args.capture_input, start_appearance_capture, finish_appearance_capture, activate_owned_process)
        return
    if args.appearance or args.capture_codec_experiment or args.capture_bgra_experiment:
        if any(artifacts.iterdir()):
            raise RuntimeError('Appearance capture requires an empty artifact directory')
        if not args.system_theme:
            raise RuntimeError('Record the selected system theme explicitly')
        dark = subprocess.check_output(['osascript', '-e', 'tell application "System Events" to tell appearance preferences to get dark mode'], text=True).strip() == 'true'
        if dark != (args.system_theme == 'dark'):
            raise RuntimeError('Observed macOS appearance does not match requested system theme')
        if args.capture_codec_experiment or args.capture_bgra_experiment:
            observations = []
            for codec in (['copy'] if args.capture_bgra_experiment else ['ffv1', 'copy']):
                directory = artifacts / ('copy-bgr0' if args.capture_bgra_experiment else codec)
                directory.mkdir()
                label = 'appearance-app-light'
                outcome = {'codec': codec, 'captureSeconds': 6, 'runtimePassed': False, 'capturePassed': False}
                if args.capture_bgra_experiment:
                    outcome['requestedInputPixelFormat'] = 'bgr0'
                capture = None
                try:
                    run_phase(app, 'appearance-seed-light', directory, [])
                    capture = start_appearance_capture(directory, label, 'macos', args.capture_input,
                                                       experiment_codec=codec, bgra_experiment=args.capture_bgra_experiment)
                except Exception as error:
                    outcome['preparationError'] = {'type': type(error).__name__, 'message': str(error)}
                if capture is not None:
                    try:
                        outcome['runtime'] = run_phase(app, label, directory, [])
                        outcome['runtimePassed'] = True
                    except Exception as error:
                        outcome['runtimeError'] = {'type': type(error).__name__, 'message': str(error)}
                    try:
                        outcome['capture'] = finish_appearance_capture(capture)
                        progress = (directory / f'{label}-capture-progress.txt').read_text()
                        duplicates = re.findall(r'^dup_frames=(\d+)$', progress, re.M)
                        drops = re.findall(r'^drop_frames=(\d+)$', progress, re.M)
                        if not duplicates or not drops or int(duplicates[-1]) != 0 or int(drops[-1]) != 0:
                            raise RuntimeError('Capture output duplicated or dropped frames')
                        outcome['capturePassed'] = True
                    except Exception as error:
                        outcome['captureError'] = {'type': type(error).__name__, 'message': str(error)}
                        metadata = capture['video'].with_suffix('.json')
                        if metadata.exists():
                            outcome['capture'] = json.loads(metadata.read_text())
                log = directory / f'{label}-capture.log'
                if log.exists():
                    text = log.read_text()
                    if args.capture_bgra_experiment:
                        outcome['inputFormatConfirmed'] = bool(re.search(r'Video: rawvideo .*?, bgr0,', text))
                    statistics = re.search(r'bench: utime=([\d.]+)s stime=([\d.]+)s rtime=([\d.]+)s', text)
                    if statistics:
                        outcome['benchmarkSeconds'] = dict(zip(['user', 'system', 'elapsed'], map(float, statistics.groups())))
                if args.capture_bgra_experiment and not outcome.get('inputFormatConfirmed'):
                    outcome['capturePassed'] = False
                    outcome['inputFormatError'] = 'Requested bgr0 input was not confirmed'
                observations.append(outcome)
            report_name = 'capture-bgra-experiment' if args.capture_bgra_experiment else 'capture-codec-experiment'
            (artifacts / f'{report_name}.json').write_text(json.dumps({
                'diagnosticOnly': True, 'systemTheme': 'light', 'appTheme': 'light',
                'observations': observations}, indent=2))
            report_key = 'captureBgraExperiment' if args.capture_bgra_experiment else 'captureCodecExperiment'
            print(json.dumps({report_key: observations}), flush=True)
            if not all(item['runtimePassed'] and item['capturePassed'] for item in observations):
                raise RuntimeError('BGRA capture experiment failed' if args.capture_bgra_experiment else
                                   'Paired capture experiment failed one or more controls')
            return
        observations = []
        for theme in ['light', 'dark']:
            run_phase(app, f'appearance-seed-{theme}', artifacts, [])
            label = f'appearance-app-{theme}'
            capture = start_appearance_capture(artifacts, label, 'macos', args.capture_input)
            runtime_error = None
            try:
                observation = run_phase(app, label, artifacts, [])
            except BaseException as error:
                runtime_error = error
                raise
            finally:
                try:
                    capture_result = finish_appearance_capture(capture)
                except BaseException as capture_error:
                    secondary = {'type': type(capture_error).__name__, 'message': str(capture_error)}
                    try:
                        (artifacts / f'{label}-capture-failure.json').write_text(json.dumps(secondary, indent=2))
                    except OSError as report_error:
                        print(f'{label}: capture failure report could not be saved ({type(report_error).__name__})', file=sys.stderr, flush=True)
                    if runtime_error is None:
                        raise
                    print(f'{label}: secondary capture failure: {json.dumps(secondary)}', file=sys.stderr, flush=True)
            observations.append({'appTheme': theme, 'systemTheme': args.system_theme,
                                 'runtime': observation, 'capture': capture_result})
        (artifacts / 'appearance-result.json').write_text(json.dumps({'visualReview': 'required', 'observations': observations}, indent=2))
        print('Appearance captures complete; content background and title bar require separate visual review', flush=True)
        return
    fixture_root = Path(tempfile.mkdtemp(prefix='risunest-macos-', dir='/private/tmp'))
    fixtures = [fixture_root / 'synthetic 한글 # %.risup', fixture_root / 'synthetic-two.risum']
    for fixture in fixtures:
        fixture.write_text('synthetic file association fixture')
    configured_phases = os.environ.get('RISUNEST_MACOS_PHASES')
    phases = configured_phases.split(',') if configured_phases else [
        'contracts', 'restart', 'app', 'app-restart', 'streaming',
        'session-dispatch-initial', 'session-dispatch-local', 'session-dispatch-drain', 'session-dispatch-dialog',
        'quit-escape', 'session-deadline', 'session-upgrade',
    ]
    allowed_phases = {'session-upgrade', 'session-deadline', 'termination-probe', 'contracts', 'restart', 'app', 'app-restart', 'streaming', 'quit-escape',
                      'session-dispatch-initial', 'session-dispatch-local', 'session-dispatch-drain', 'session-dispatch-dialog',
                      'appearance-seed-light', 'appearance-app-light', 'appearance-seed-dark', 'appearance-app-dark',
                      *SYNC_PHASES, *EXTERNAL_PHASES}
    if not phases or any(phase not in allowed_phases for phase in phases):
        raise RuntimeError('invalid RISUNEST_MACOS_PHASES')
    if any(phase in SYNC_PHASES for phase in phases):
        # Each phase is one device of a single round trip on one loopback server.
        if [phase for phase in phases if phase in SYNC_PHASES] != list(SYNC_PHASES):
            raise RuntimeError('Sync phases run together and in order')
        if os.environ.get('GITHUB_ACTIONS') != 'true':
            raise RuntimeError('The Sync round trip is restricted to a fresh hosted CI user')
        if not args.sync_server:
            parser.error('Sync phases require --sync-server')
    external_phases = [phase for phase in phases if phase in EXTERNAL_PHASES]
    if external_phases:
        if not any(external_phases == list(group) for group in EXTERNAL_GROUPS):
            raise RuntimeError('External storage phases run as one group, together and in order')
        if os.environ.get('GITHUB_ACTIONS') != 'true':
            raise RuntimeError('The external storage round trip is restricted to a fresh hosted CI user')
        if not args.webdav_server and external_phases[0] not in FILE_PHASES:
            parser.error('External storage phases require --webdav-server')
    # A failed phase skips only the phases that read the data it leaves; the rest still run.
    dependencies = {'restart': 'contracts', 'app': 'contracts', 'app-restart': 'app',
                    'appearance-app-light': 'appearance-seed-light', 'appearance-app-dark': 'appearance-seed-dark',
                    **SYNC_DEPENDENCIES, **EXTERNAL_DEPENDENCIES}
    results = {}
    failures = {}
    sync = None
    external = None
    backup_file = file_markers()
    try:
        for phase in phases:
            if dependencies.get(phase) in failures:
                failures[phase] = f'skipped because {dependencies[phase]} did not pass'
                print(f'{phase}: {failures[phase]}', flush=True)
                continue
            launched = False
            try:
                expected = None
                if phase == 'restart' and 'contracts' in results:
                    expected = next(entry['result'] for entry in results['contracts'] if entry['stage'] == 'quit-saved')
                if phase == 'app-restart' and 'app' in results:
                    expected = next(entry['result'] for entry in results['app'] if entry['stage'] == 'app-native-saved')
                if phase in SYNC_PHASES:
                    if sync is None:
                        sync = SyncSession(args.sync_server, artifacts)
                        sync.start()
                    expected = sync.before(phase)
                if phase in FILE_PHASES:
                    expected = backup_file
                elif phase in EXTERNAL_PHASES:
                    if external is None:
                        external = ExternalStorageSession(args.webdav_server, artifacts, args.webdav_scheme)
                        external.start()
                    expected = external.before(phase)
                launched = True
                results[phase] = run_phase(app, phase, artifacts, fixtures, expected)
            except SyncEnvironmentError as error:
                failures[phase] = f'sync-env: {error}'
                print(f'{phase}: FAILED {failures[phase]}', flush=True)
            except Exception as error:
                failures[phase] = f'{type(error).__name__}: {error}'
                print(f'{phase}: FAILED {failures[phase]}', flush=True)
            if launched and phase in SYNC_PHASES:
                failure = sync.summarize(phase)
                if failure and phase in failures:
                    failures[phase] = f"{failure['label']}: {failure['step']}: {failure['message']}"
        if 'app' in results and 'app-restart' in results:
            saved = next(entry['result']['revision'] for entry in results['app'] if entry['stage'] == 'app-native-saved')
            restarted = next(entry['result']['revision'] for entry in results['app-restart'] if entry['stage'] == 'app-restart')
            if saved != restarted:
                failures['app-restart'] = 'Product restart revision differs from the native-approved saved revision'
        (artifacts / 'result.json').write_text(json.dumps({'passed': not failures, 'phases': results, 'failures': failures}, indent=2))
    finally:
        if sync is not None:
            sync.close()
        if external is not None:
            external.close()
    if failures:
        raise RuntimeError(f'Failed phases: {json.dumps(failures, indent=2)}')
    print('Mac WKWebView contracts, restart and product app passed', flush=True)


if __name__ == '__main__':
    main()
