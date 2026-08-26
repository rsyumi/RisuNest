import { gzipSync } from 'node:zlib'
import { readFile, writeFile } from 'node:fs/promises'
import path from 'node:path'
import process from 'node:process'
import { pathToFileURL } from 'node:url'

const HIGHLIGHT_SOURCE = /(?:^|[/\\])highlight\.js(?:[/\\]|$)/
const SORTABLE_SOURCE = /(?:^|[/\\])sortablejs(?:[/\\]|$)/

export function resolveInitialFiles(manifest, entryKey = 'index.html') {
    const visitedEntries = new Set()
    const files = new Set()

    function visit(key) {
        if (visitedEntries.has(key)) return
        const entry = manifest[key]
        if (!entry) throw new Error(`Missing manifest entry: ${key}`)
        visitedEntries.add(key)
        if (entry.file) files.add(entry.file)
        for (const css of entry.css ?? []) files.add(css)
        for (const importedKey of entry.imports ?? []) visit(importedKey)
    }

    visit(entryKey)
    return [...files].sort()
}

export function percentileNearestRank(samples, percentile) {
    if (samples.length === 0) throw new Error('Cannot calculate a percentile without samples')
    if (!(percentile > 0 && percentile <= 1)) throw new Error('Percentile must be in (0, 1]')
    const sorted = [...samples].sort((left, right) => left - right)
    return sorted[Math.ceil(sorted.length * percentile) - 1]
}

export function decideHighlightGate({ baselineGzipBytes, candidateGzipBytes, firstUseP95Ms }) {
    const savedGzipBytes = baselineGzipBytes - candidateGzipBytes
    const savedPercent = baselineGzipBytes === 0 ? 0 : savedGzipBytes / baselineGzipBytes * 100
    const sizeGatePassed = savedGzipBytes >= 30 * 1024 || savedPercent >= 2
    const latencyGatePassed = firstUseP95Ms < 100
    return {
        adopt: sizeGatePassed && latencyGatePassed,
        savedGzipBytes,
        savedPercent,
        sizeGatePassed,
        latencyGatePassed,
    }
}

export function classifyPackageSources(sources) {
    const normalizedSources = sources.map((source) => source.replaceAll('\\', '/'))
    return {
        highlight: normalizedSources.some((source) => HIGHLIGHT_SOURCE.test(source)),
        sortable: normalizedSources.some((source) => SORTABLE_SOURCE.test(source)),
    }
}

async function packagePresence(distDirectory, files) {
    const result = { highlight: [], sortable: [], sourceMapsMissing: [] }
    for (const file of files.filter((name) => name.endsWith('.js'))) {
        const sourceMapPath = path.join(distDirectory, `${file}.map`)
        let sourceMap
        try {
            sourceMap = JSON.parse(await readFile(sourceMapPath, 'utf8'))
        } catch (error) {
            if (error?.code === 'ENOENT') {
                result.sourceMapsMissing.push(file)
                continue
            }
            throw error
        }
        const presence = classifyPackageSources(sourceMap.sources ?? [])
        if (presence.highlight) result.highlight.push(file)
        if (presence.sortable) result.sortable.push(file)
    }
    return result
}

export async function measureInitialGraph(distDirectory) {
    const manifestPath = path.join(distDirectory, '.vite', 'manifest.json')
    const manifest = JSON.parse(await readFile(manifestPath, 'utf8'))
    const files = resolveInitialFiles(manifest)
    let rawBytes = 0
    let gzipBytes = 0
    const entries = []

    for (const file of files) {
        const bytes = await readFile(path.join(distDirectory, file))
        const compressedBytes = gzipSync(bytes, { level: 9 }).byteLength
        rawBytes += bytes.byteLength
        gzipBytes += compressedBytes
        entries.push({ file, rawBytes: bytes.byteLength, gzipBytes: compressedBytes })
    }

    return {
        entry: 'index.html',
        rawBytes,
        gzipBytes,
        files: entries,
        startupPackages: await packagePresence(distDirectory, files),
    }
}

async function main() {
    const args = process.argv.slice(2)
    const distIndex = args.indexOf('--dist')
    const outputIndex = args.indexOf('--output')
    const distDirectory = path.resolve(distIndex === -1 ? 'dist' : args[distIndex + 1])
    const result = await measureInitialGraph(distDirectory)
    const json = `${JSON.stringify(result, null, 2)}\n`
    if (outputIndex !== -1) await writeFile(path.resolve(args[outputIndex + 1]), json, 'utf8')
    process.stdout.write(json)
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
    main().catch((error) => {
        process.stderr.write(`${error.stack ?? error}\n`)
        process.exitCode = 1
    })
}
