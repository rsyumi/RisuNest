import { readFile } from 'node:fs/promises'
import { percentile } from './metrics.mjs'

const input = JSON.parse(await readFile(process.argv[2], 'utf8'))
const distribution = (values) => {
    if (!values.length || values.some((value) => !Number.isFinite(value)))
        throw new Error('Missing numeric observation')
    return {
        count: values.length,
        p50: percentile(values, 50),
        p95: percentile(values, 95),
        p99: percentile(values, 99),
        max: percentile(values, 100),
    }
}
const android = input.platform === 'android-arm64'
const result = {
    revision: input.revision,
    binarySha256: input.binarySha256,
    platform: input.platform ?? 'windows',
    groups: {},
}
for (const kind of ['reload', 'restart']) {
    const rows = input.samples.filter((row) => !row.warmup && row.kind === kind)
    if (rows.length < 20) throw new Error('M0 requires twenty samples per group')
    if (
        rows.some(
            (row) =>
                !row.interaction?.success ||
                row.interaction.failure !== null ||
                !row.interaction.inputAccepted ||
                !row.interaction.scrollImmediateChanged ||
                row.documentVisible !== true ||
                row.stabilizationTimeout ||
                !row.host ||
                !row.calls.some((call) => call.command === 'pds_commit' && call.success) ||
                row.calls.some((call) => call.command === 'pds_commit' && !call.success),
        )
    ) {
        throw new Error('Invalid M0 sample')
    }
    const commits = rows.flatMap((row) =>
        row.calls.filter((call) => call.command === 'pds_commit').map((call) => call.ms),
    )
    if (commits.length < rows.length) throw new Error('Missing local commit observations')
    result.groups[kind] = {
        samples: rows.length,
        inputMs: distribution(rows.map((row) => row.interaction.inputMs)),
        scrollMs: distribution(rows.map((row) => row.interaction.scrollMs)),
        commitIpcMs: distribution(commits),
        longTaskCount: distribution(rows.map((row) => row.longTasks.length)),
        longestTaskMs: distribution(
            rows.map((row) => Math.max(0, ...row.longTasks.map((task) => task.ms))),
        ),
        jsHeapBytes: distribution(rows.map((row) => row.usedHeapBytes)),
        ...(android
            ? {
                  appPssBytes: distribution(rows.map((row) => row.host.memory.appPssBytes)),
                  appRssBytes: distribution(rows.map((row) => row.host.memory.appRssBytes)),
                  walStartBytes: distribution(rows.map((row) => row.host.wal.startBytes)),
              }
            : {
                  nativeWorkingSetBytes: distribution(
                      rows.map((row) => row.host.memory.nativeWorkingSetBytes),
                  ),
                  nativePeakWorkingSetBytes: distribution(
                      rows.map((row) => row.host.memory.nativePeakWorkingSetBytes),
                  ),
                  treeWorkingSetBytes: distribution(
                      rows.map((row) => row.host.memory.treeWorkingSetBytes),
                  ),
                  walPeakBytes: distribution(rows.map((row) => row.host.wal.peakBytes)),
              }),
        walEndBytes: distribution(rows.map((row) => row.host.wal.endBytes)),
    }
}
process.stdout.write(JSON.stringify(result, null, 2) + '\n')
