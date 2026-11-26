import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdir, readFile, writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('../../', import.meta.url))
const output = path.resolve(root, process.argv[2] || '.tmp/generation-send')
const repetitions = Number(process.env.GENERATION_SEND_REPETITIONS || 3)
if (!Number.isSafeInteger(repetitions) || repetitions < 1) throw new Error('Expected a positive repetition count')
const reference = process.argv[3] ? JSON.parse((await readFile(path.resolve(root, process.argv[3]), 'utf8')).replace(/^\uFEFF/, '')) : null
const referenceRows = reference ? (Array.isArray(reference) ? reference : reference.rows) : []
if (!Array.isArray(referenceRows)) throw new Error('Expected reference rows')
let comparedCases = 0
await mkdir(output, { recursive: true })
const sources = ['src/ts/process/index.svelte.ts', 'src/ts/process/scripts.ts',
    'src/ts/parser/parser.svelte.ts', 'src/ts/tokenizer.ts', 'src/ts/storage/saveCoordinator.ts',
    'src/ts/process/promptHistory.ts', 'src/ts/process/historyWindowHistory.ts',
    'src/ts/storage/conversationHistoryOperation.ts', 'src/ts/storage/activeConversationSession.ts',
    'src/ts/storage/windowedConversationOverlay.ts', 'src/ts/storage/activeWorkingSet.svelte.ts',
    'src/ts/process/generationResponseApplication.ts', 'src/ts/generationDiagnostics.ts',
    'src/ts/process/request/request.ts',
    'src/ts/process/triggers.ts', 'benchmarks/generation-send/send.test.ts',
    'benchmarks/generation-send/probe.ts', 'benchmarks/generation-send/run.mjs',
    'benchmarks/generation-memory/fixture.ts']
const sourceSha256 = Object.fromEntries(await Promise.all(sources.map(async source =>
    [source, createHash('sha256').update(await readFile(path.join(root, source))).digest('hex')]
)))
const rows = []
for (let repetition = 1; repetition <= repetitions; repetition++) {
    for (const historyTokens of [100000, 400000, 1600000]) {
        for (const historyLimit of [false, true]) {
            const name = `${historyTokens}-${historyLimit ? 'window' : 'complete'}-${repetition}`
            process.stdout.write(`Running ${name}\n`)
            const result = await new Promise((resolve, reject) => {
                const child = spawn(process.execPath, [path.join(root, 'node_modules/vitest/vitest.mjs'),
                    'run', '--config', 'benchmarks/generation-send/vitest.config.ts'], {
                    cwd: root, windowsHide: true,
                    env: { ...process.env, GENERATION_SEND_TOKENS: String(historyTokens),
                        GENERATION_SEND_WINDOW: historyLimit ? '1' : '0' },
                    stdio: ['ignore', 'pipe', 'pipe'],
                })
                let log = ''
                child.stdout.on('data', data => { log += data })
                child.stderr.on('data', data => { log += data })
                child.on('error', reject)
                child.on('exit', code => resolve({ code, log }))
            })
            await writeFile(path.join(output, `${name}.log`), result.log)
            if (result.code !== 0) throw new Error(`${name} failed; see its log`)
            const marker = result.log.split(/\r?\n/).find(line => line.startsWith('GENERATION_SEND '))
            if (!marker) throw new Error(`${name} did not emit its measurement`)
            const row = { repetition, ...JSON.parse(marker.slice('GENERATION_SEND '.length)) }
            if (!row.phases.tokenizer || !row.phases['current-chat-parser']
                || !row.phases[historyLimit ? 'history-window' : 'complete-promotion']) {
                throw new Error(name + ' is missing required phase probes')
            }
            const expected = referenceRows.find(expected => expected.historyTokens === row.historyTokens
                && expected.historyLimit === row.historyLimit && Boolean(expected.templateEnabled) === row.templateEnabled
                && expected.promptSha256)
            if (expected) {
                for (const key of ['promptSha256', 'inputTokens', 'promptMessages', 'promptCharacters', 'responseTokens', 'saveRouteAtProvider']) {
                    if (row[key] !== expected[key]) throw new Error(`${name} changed reference ${key}`)
                }
                comparedCases++
            }
            rows.push(row)
            await writeFile(path.join(output, 'results.json'), JSON.stringify({
                schema: 'generation-send-baseline/v1', platform: process.platform, node: process.version,
                sourceSha256, comparedCases, policy: 'Fresh worker per case; tokenizer WASM warmed; global tokenizer string-result cache disabled; no full-send warmup; serial runs',
                rows,
            }, null, 2) + '\n')
            process.stdout.write(JSON.stringify({ name, preparationMs: row.preparationMs,
                completionMs: row.completionMs, maxEventLoopGapMs: row.maxEventLoopGapMs }) + '\n')
        }
    }
}
for (const source of sources) {
    const current = createHash('sha256').update(await readFile(path.join(root, source))).digest('hex')
    if (current !== sourceSha256[source]) throw new Error(`Product source changed during the baseline: ${source}`)
}
const median = values => {
    const sorted = [...values].sort((a, b) => a - b)
    const middle = Math.floor(sorted.length / 2)
    return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2
}
const groups = new Map()
for (const row of rows) {
    const key = row.historyTokens + ':' + row.generationRoute
    const group = groups.get(key) || []
    group.push(row)
    groups.set(key, group)
}
const summary = [...groups.values()].map(group => ({
    historyTokens: group[0].historyTokens, generationRoute: group[0].generationRoute,
    templateEnabled: group[0].templateEnabled, probeMode: group[0].probeMode, observations: group.length,
    preparationMs: median(group.map(row => row.preparationMs)),
    completionMs: median(group.map(row => row.completionMs)),
    preparationEncodingMs: median(group.map(row => row.phases.tokenizer.preparationMs)),
    preparationOtherMs: median(group.map(row => row.preparationMs - row.phases.tokenizer.preparationMs)),
    maxEventLoopGapMs: median(group.map(row => row.maxEventLoopGapMs)),
    encodeCalls: median(group.map(row => row.phases.tokenizer.calls)),
    promptSha256: [...new Set(group.map(row => row.promptSha256))],
    inputTokens: [...new Set(group.map(row => row.inputTokens))],
}))
await writeFile(path.join(output, 'summary.json'), JSON.stringify({ sourceSha256, comparedCases, summary }, null, 2) + '\n')
process.stdout.write(`Saved ${rows.length} cases; ${comparedCases} compared with exact reference outputs\n`)
