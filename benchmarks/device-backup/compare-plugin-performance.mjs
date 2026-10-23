import { readFile, writeFile } from 'node:fs/promises'

const [baselinePath, candidatePath, output] = process.argv.slice(2)
if (!baselinePath || !candidatePath) throw new Error('Usage: node compare-plugin-performance.mjs baseline.json candidate.json [report.md]')
const reports = await Promise.all([baselinePath, candidatePath].map(async file => JSON.parse(await readFile(file, 'utf8'))))
for (const report of reports) {
  if (!report.passed || !report.identityVerified || report.pluginPerformance?.phase !== 'done') throw new Error('A complete, identity-verified performance result is required')
}
for (const file of ['benchmarks/device-backup/main.ts', 'benchmarks/device-backup/plugin-performance.ts']) {
  if (reports[0].sourceHashes[file] !== reports[1].sourceHashes[file]) throw new Error(`Different measurement source: ${file}`)
}
const baseline = new Map(reports[0].pluginPerformance.results.map(row => [row.name, row]))
const format = value => value.toFixed(value < 0.1 ? 5 : 2)
const rows = ['| Operation | Before ms | After ms | Change | After min–max ms |', '|---|---:|---:|---:|---:|']
for (const candidate of reports[1].pluginPerformance.results) {
  if (!Number.isFinite(candidate.medianMs)) continue
  const before = baseline.get(candidate.name)
  rows.push(`| ${candidate.name} | ${before ? format(before.medianMs) : '—'} | ${format(candidate.medianMs)} | ${before ? `${(100 * (candidate.medianMs / before.medianMs - 1)).toFixed(1)}%` : 'new supported path'} | ${format(candidate.minMs)}–${format(candidate.maxMs)} |`)
}
const correctness = reports.map(report => report.pluginPerformance.results.filter(row => row.name.endsWith('/correctness')))
const binary = correctness.every(rows => rows.length > 0) ? `\n\nBinary correctness (type and every byte): before ${correctness[0].filter(row => row.preservedType && row.preservedBytes).length}/${correctness[0].length}, after ${correctness[1].filter(row => row.preservedType && row.preservedBytes).length}/${correctness[1].length}.` : ''
const text = `${rows.join('\n')}${binary}\n\nTimings are medians of seven batch-average samples after two warm-up batches, using Windows debug agents; no iframe RPC, backup or sync. Cold means frontend cache invalidated, not physical disk cold.\n`
if (output) await writeFile(output, text)
else process.stdout.write(text)
