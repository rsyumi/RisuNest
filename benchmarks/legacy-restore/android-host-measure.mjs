import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { mkdir, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

const terminalPhases = new Set(['verified', 'failed']);
const fixtureScript = fileURLToPath(new URL('./android-host-fixture.mjs', import.meta.url));

// Runs in the synthetic page: the same commands and polling as run.ts, then a bounded verification.
function pageRunner(config) {
  return `(() => {
    const config = ${JSON.stringify(config)};
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
    const terminal = new Set(['succeeded', 'failed', 'cancelled']);
    const hex = async text => Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text))),
      byte => byte.toString(16).padStart(2, '0')).join('');
    const projection = messages => JSON.stringify(messages.map(({ role, chatId, data }) => [role, chatId, data]));
    if (globalThis.__hostLegacyRestore) throw new Error('Restart the synthetic harness for each memory case');
    const runner = globalThis.__hostLegacyRestore = { state: { phase: 'opening' } };
    const set = next => { runner.state = next; };
    void (async () => {
      try {
        const opened = await invoke('pds_open');
        const startedAt = Date.now();
        set({ phase: 'restore-started', startedAt, revision: opened.revision });
        const job = await invoke('native_file_job_start', { request: {
          kind: 'restore-legacy-local-backup', source: { type: 'desktopPath', path: config.path }, expectedRevision: opened.revision,
        } });
        let status;
        let finalized = false;
        let polls = 0;
        const jobPhases = [];
        do {
          status = await invoke('native_file_job_status', { jobId: job.jobId });
          polls++;
          if (jobPhases.at(-1)?.[0] !== status.phase) jobPhases.push([status.phase, Date.now() - startedAt]);
          if (status.phase === 'awaiting-activation' && !finalized) {
            await invoke('native_file_job_finalize', { jobId: job.jobId, expectedRevision: opened.revision });
            finalized = true;
          }
          if (!terminal.has(status.state)) {
            set({ phase: 'restore-started', startedAt, revision: opened.revision, jobPhase: status.phase, polls });
            await pause(200);
          }
        } while (!terminal.has(status.state));
        const finishedAt = Date.now();
        const measurement = { phase: 'restore-terminal', startedAt, finishedAt, elapsedMs: finishedAt - startedAt,
          outcome: status.state, code: status.error?.code ?? null,
          error: status.error ? JSON.stringify(status.error).slice(0, 2048) : null,
          result: status.result ? { characterCount: status.result.characterCount, sourceBytes: status.result.sourceBytes } : null,
          jobPhases, polls };
        set(measurement);
        if (status.state !== 'succeeded') return;
        // Verification waits until the host has seen the terminal state, so the window ends there.
        while (!runner.acknowledged) await pause(50);
        let messages = 0;
        let verified = 0;
        for (const expected of config.conversations) {
          const read = await invoke('pds_read_conversation', { characterId: expected.characterId, conversationId: expected.conversationId });
          if (!read || await hex(projection(read.value.message)) !== expected.sha256)
            throw new Error('Restored synthetic message hash mismatch');
          messages += read.value.message.length;
          verified++;
          set({ ...measurement, phase: 'verifying', verifiedCharacterCount: verified });
        }
        let binaryVerification = null;
        if (config.binary?.mode === 'read') {
          // A bin is stored as the object JavaScript makes of a Uint8Array, one member per byte.
          const read = await invoke('pds_read_character', { id: config.binary.characterId });
          const text = JSON.stringify(read?.value.syntheticBinary);
          if (typeof text !== 'string' || await hex(text) !== config.binary.sha256) throw new Error('Restored synthetic binary mismatch');
          binaryVerification = { mode: 'read', characterId: config.binary.characterId, storedTextLength: text.length };
        } else if (config.binary?.mode === 'summary') {
          // Reading a multi-megabyte bin back would hold its stored text in this page; only presence is checked here.
          const summary = await invoke('pds_read_character_summary', { id: config.binary.characterId });
          if (!summary) throw new Error('Restored synthetic binary character missing');
          binaryVerification = { mode: 'summary', characterId: config.binary.characterId };
        }
        await invoke('native_file_job_forget', { jobId: job.jobId });
        set({ ...measurement, phase: 'verified', verifiedMessageCount: messages, verifiedCharacterCount: verified, binaryVerification });
      } catch (error) {
        set({ ...runner.state, phase: 'failed', failure: String(error?.message ?? error).slice(0, 1024) });
      }
    })();
    return true;
  })()`;
}

/**
 * Builds the legacy fixture on the host, pushes it into the synthetic app's files and drives the restore
 * through the app's commands, so the in-app harness of the installed APK only has to reach `ready`.
 * `--legacy-single-binary-bytes=<n>` places one bin of n bytes in the last character of the smallest
 * fixture instead of a decoded-size case.
 */
export async function measureHostLegacyRestore({ args, client, run, pid, packageName, output, apkSha256, environment }) {
  const encoding = args['legacy-encoding'] ?? 'raw';
  assert.equal(encoding, 'raw', 'Host fixtures are raw RisuSave');
  assert.ok(!args.peer, 'Legacy memory cases require a fresh synthetic process');
  const runId = randomUUID();
  const directory = path.join(path.dirname(output), `host-fixture-${runId}`);
  await mkdir(directory, { recursive: true });
  const hostFile = path.join(directory, 'synthetic.bin');
  const deviceName = `synthetic-legacy-${runId}.bin`;
  const temporary = `/data/local/tmp/${deviceName}`;
  const native = `/data/user/0/${packageName}/files/${deviceName}`;
  const events = [];
  let latest = { phase: 'preparing' };
  const identity = { schema: 'risunest.synthetic-legacy-restore-host/v1', platform: 'android-emulator', synthetic: true,
    apkSha256, environment, runId, encoding };
  let fixtureInfo = null;
  let restoreBaselineBytes = null;
  let restoreResidentMaxBytes = null;
  let peakRssBytes = null;
  let hwmAtTerminalBytes = null;
  let samples = 0;
  let processExited = false;
  const report = () => ({ ...identity, ...fixtureInfo, ...latest, memorySamples: samples,
    memorySource: 'linux-proc-VmRSS-and-VmHWM-kib-converted-to-bytes', restoreBaselineBytes, restoreResidentMaxBytes,
    restoreIncrementBytes: restoreBaselineBytes === null ? null : restoreResidentMaxBytes - restoreBaselineBytes,
    peakRssBytes, hwmAtTerminalBytes, peakScope: 'native-process-lifetime-high-water; the fixture is written by the host',
    processExited, events });
  const save = () => writeFile(output, JSON.stringify(report(), null, 2));
  const mark = async (phase, extra = {}) => {
    events.push({ phase, hostObservedAt: Date.now(), ...extra });
    await save();
  };
  try {
    await mark('fixture-writing');
    const { stdout } = await promisify(execFile)(process.execPath, [fixtureScript, hostFile, runId,
      String(Number(args['legacy-mb'] ?? 0)), String(Number(args['legacy-binary-bytes'] ?? 0)),
      String(Number(args['legacy-single-binary-bytes'] ?? 0))], { windowsHide: true, timeout: 600_000, maxBuffer: 16 * 1024 * 1024 });
    const { conversations, binary, ...built } = JSON.parse(stdout);
    fixtureInfo = { ...built, binaryCheck: binary };
    const fileSha256 = built.fixtureSha256;
    await mark('fixture-written');
    try {
      await run(['push', hostFile, temporary], 600_000);
      await run(['shell', 'run-as', packageName, 'cp', temporary, native], 600_000);
    } finally { await run(['shell', 'rm', '-f', temporary], 10_000, true); }
    const deviceSha256 = (await run(['shell', 'run-as', packageName, 'sha256sum', native], 120_000)).stdout.trim().split(/\s/)[0];
    assert.equal(deviceSha256, fileSha256, 'Pushed synthetic fixture bytes changed');
    await rm(directory, { recursive: true, force: true });
    await mark('fixture-pushed');
    const status = async () => {
      const text = await run(['shell', 'run-as', packageName, 'cat', `/proc/${pid}/status`], 5000, true);
      const hwm = /^VmHWM:\s*(\d+)\s+kB$/m.exec(text.stdout);
      const rss = /^VmRSS:\s*(\d+)\s+kB$/m.exec(text.stdout);
      return hwm && rss ? { resident: Number(rss[1]) * 1024, hwm: Number(hwm[1]) * 1024 } : null;
    };
    const before = await status();
    await mark('before-restore', { residentBytes: before?.resident ?? null, hwmBytes: before?.hwm ?? null });
    assert.equal(await client.evaluate(pageRunner({ path: native, conversations, binary })), true);
    const deadline = Date.now() + 30 * 60_000;
    let lastPhase;
    while (Date.now() < deadline) {
      const currentPid = (await run(['shell', 'pidof', packageName], 5000, true)).stdout.trim();
      if (currentPid !== pid) { processExited = true; await mark('process-exited', { pid: currentPid || null }); break; }
      const next = await client.evaluate('globalThis.__hostLegacyRestore?.state ?? null').catch(() => null);
      if (next) latest = next;
      if (latest.phase === 'restore-started' || lastPhase === 'restore-started') {
        const sample = await status();
        if (sample) {
          if (restoreBaselineBytes === null) restoreBaselineBytes = sample.resident;
          restoreResidentMaxBytes = Math.max(restoreResidentMaxBytes ?? 0, sample.resident);
          peakRssBytes = Math.max(peakRssBytes ?? 0, sample.hwm);
          if (latest.phase !== 'restore-started') hwmAtTerminalBytes = sample.hwm;
          samples++;
        }
      }
      if (latest.phase !== lastPhase) {
        lastPhase = latest.phase;
        await mark(latest.phase);
        if (latest.phase === 'restore-terminal') await client.evaluate('globalThis.__hostLegacyRestore.acknowledged = true');
      }
      if (terminalPhases.has(latest.phase) || latest.phase === 'restore-terminal' && latest.outcome !== 'succeeded') break;
      await new Promise(resolve => setTimeout(resolve, 250));
    }
    const result = report();
    result.success = latest.phase === 'verified' && latest.result?.characterCount === fixtureInfo.characterCount
      && latest.result?.sourceBytes === fixtureInfo.sourceBytes && latest.verifiedMessageCount === fixtureInfo.messageCount;
    if (!result.success && !processExited && !terminalPhases.has(latest.phase) && latest.phase !== 'restore-terminal')
      result.incomplete = true;
    await writeFile(output, JSON.stringify(result, null, 2));
    return result;
  } finally {
    await rm(directory, { recursive: true, force: true });
    if (!processExited) await run(['shell', 'run-as', packageName, 'rm', '-f', native], 10_000, true);
  }
}
