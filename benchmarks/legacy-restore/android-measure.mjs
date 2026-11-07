import assert from 'node:assert/strict';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';

export async function measureLegacyRestore({ args, client, run, pid, packageName, output, apkSha256, environment }) {
  const megabytes = Number(args['legacy-mb']);
  const encoding = args['legacy-encoding'];
  const binaryBytes = Number(args['legacy-binary-bytes'] ?? 0);
  assert.ok([100, 300, 600].includes(megabytes) && ['raw', 'gzip'].includes(encoding), 'Explicit legacy MB and encoding required');
  assert.ok(Number.isSafeInteger(binaryBytes) && binaryBytes >= 0, 'Legacy binary bytes must be a whole number');
  assert.ok(!args.peer, 'Legacy memory cases require a fresh synthetic process');
  const records = [];
  const decodedBytes = megabytes * 1_000_000;
  let latest = { phase: 'preparing', decodedBytes, encoding, binaryBytes };
  let peakRssBytes = null;
  let restoreBaselineBytes = null;
  let restoreResidentMaxBytes = null;
  let samples = 0;
  let processExited = false;
  let memorySource = null;
  const increment = () => restoreBaselineBytes === null ? null : restoreResidentMaxBytes - restoreBaselineBytes;
  const report = () => ({ schema: 'risunest.synthetic-legacy-restore/v1', platform: 'android-emulator',
    synthetic: true, apkSha256, environment, ...latest, memorySamples: samples, memorySource,
    memoryScope: 'native-process-resident-increment-from-restore-start-excludes-separate-webcontent',
    restoreBaselineBytes, restoreResidentMaxBytes, restoreIncrementBytes: increment(),
    incrementToDecodedRatio: increment() === null ? null : increment() / decodedBytes,
    aboveTwiceDecoded: increment() === null ? null : increment() > 2 * decodedBytes,
    peakRssBytes, peakScope: 'native-process-lifetime-high-water-includes-fixture-write',
    peakToDecodedRatio: peakRssBytes === null ? null : peakRssBytes / decodedBytes,
    peakToSourceRatio: peakRssBytes === null || !latest.sourceBytes ? null : peakRssBytes / latest.sourceBytes,
    processExited, processKillConfirmed: null, events: records });
  await mkdir(path.dirname(output), { recursive: true });
  await writeFile(output, JSON.stringify(report(), null, 2));
  await client.evaluate(`void globalThis.__legacyRestoreMeasurement.start(${megabytes}, ${JSON.stringify(encoding)}, ${binaryBytes})`, false);
  const deadline = Date.now() + 25 * 60_000;
  let lastPhase;
  while (Date.now() < deadline) {
    const currentPid = (await run(['shell', 'pidof', packageName], 5000, true)).stdout.trim();
    if (currentPid !== pid) { processExited = true; break; }
    const next = await client.evaluate('globalThis.__legacyRestoreMeasurement.state()').catch(() => null);
    if (next) latest = next;
    if (latest.phase !== lastPhase) {
      records.push({ ...latest, hostObservedAt: Date.now() });
      lastPhase = latest.phase;
      await writeFile(output, JSON.stringify(report(), null, 2));
    }
    if (latest.phase === 'restore-started' || latest.phase === 'restore-terminal') {
      const status = await run(['shell', 'run-as', packageName, 'cat', `/proc/${pid}/status`], 5000, true);
      const hwm = /^VmHWM:\s*(\d+)\s+kB$/m.exec(status.stdout);
      const rss = /^VmRSS:\s*(\d+)\s+kB$/m.exec(status.stdout);
      if (hwm && rss) {
        const resident = Number(rss[1]) * 1024;
        // The first sample after the restore starts is the baseline; the fixture is already on disk then.
        if (restoreBaselineBytes === null && latest.phase === 'restore-started') restoreBaselineBytes = resident;
        if (restoreBaselineBytes !== null) restoreResidentMaxBytes = Math.max(restoreResidentMaxBytes ?? 0, resident);
        peakRssBytes = Math.max(peakRssBytes ?? 0, Number(hwm[1]) * 1024);
        samples++;
        memorySource = 'linux-proc-VmRSS-and-VmHWM-kib-converted-to-bytes';
      }
    }
    if (['verified', 'failed'].includes(latest.phase) || latest.phase === 'restore-terminal' && latest.outcome !== 'succeeded') break;
    await new Promise(resolve => setTimeout(resolve, 250));
  }
  const result = report();
  result.success = latest.phase === 'verified' && restoreBaselineBytes !== null;
  if (!result.success && !processExited && !['failed', 'restore-terminal'].includes(latest.phase)) result.incomplete = true;
  await writeFile(output, JSON.stringify(result, null, 2));
  return result;
}
