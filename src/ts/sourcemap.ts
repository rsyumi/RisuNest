import { SourceMapConsumer } from 'source-map'
import sourceMapWasmUrl from 'source-map/lib/mappings.wasm?url'

// Use the bundled decoder so native error reports also work offline.
// @ts-expect-error initialize is a static method but typed as instance method
SourceMapConsumer.initialize({ 'lib/mappings.wasm': sourceMapWasmUrl })

const FETCH_TIMEOUT_MS = 10_000
const FAILURE_TTL_MS = 60_000
const MAX_CACHED_MAPS = 4
interface CachedMap {
    promise: Promise<SourceMapConsumer | null>
    users: number
    failedAt?: number
}
const maps = new Map<string, CachedMap>()

function discardMap(url: string, entry: CachedMap): void {
    maps.delete(url)
    void entry.promise.then((consumer) => consumer?.destroy())
}

function trimMaps(): void {
    for (const [url, entry] of maps) {
        if (maps.size <= MAX_CACHED_MAPS) break
        if (entry.users === 0) discardMap(url, entry)
    }
}

function acquireMap(url: string): CachedMap {
    let entry = maps.get(url)
    if (
        entry?.failedAt !== undefined &&
        entry.users === 0 &&
        Date.now() - entry.failedAt >= FAILURE_TTL_MS
    ) {
        discardMap(url, entry)
        entry = undefined
    }
    if (!entry) {
        const created: CachedMap = { promise: Promise.resolve(null), users: 0 }
        created.promise = (async () => {
            const controller = new AbortController()
            const timeout = setTimeout(
                () => controller.abort(),
                FETCH_TIMEOUT_MS,
            )
            try {
                const response = await fetch(url, {
                    method: 'GET',
                    signal: controller.signal,
                })
                if (!response.ok) throw new Error('Sourcemap unavailable')
                return await new SourceMapConsumer(await response.json())
            } catch {
                created.failedAt = Date.now()
                return null
            } finally {
                clearTimeout(timeout)
            }
        })()
        entry = created
    }
    entry.users += 1
    maps.delete(url)
    maps.set(url, entry)
    trimMaps()
    return entry
}

export interface StackTraceTranslationResult {
    stackTrace: string
    didTranslate: boolean
}

export async function translateStackTrace(
    stackTrace: string,
): Promise<StackTraceTranslationResult> {
    if (!stackTrace) return { stackTrace: '', didTranslate: false }
    const lines = stackTrace.split('\n')
    const linePattern = /(http[s]?:\/\/[^\s)]+\.js):(\d+):(\d+)/
    const leases = new Map<string, CachedMap>()
    for (const line of lines) {
        const match = line.match(linePattern)
        if (match && !leases.has(match[1]))
            leases.set(match[1], acquireMap(match[1] + '.map'))
    }
    try {
        const consumers = new Map(
            await Promise.all(
                [...leases].map(
                    async ([url, entry]) => [url, await entry.promise] as const,
                ),
            ),
        )
        let translated = false
        const result = lines.map((line) => {
            const match = line.match(linePattern)
            if (!match) return line
            const consumer = consumers.get(match[1])
            if (!consumer) return line
            try {
                const position = consumer.originalPositionFor({
                    line: Number(match[2]),
                    // Browser stacks count columns from one; source maps count from zero.
                    column: Math.max(0, Number(match[3]) - 1),
                })
                if (!position.source) return line
                translated = true
                const location = `${position.source}:${position.line}:${position.column}`
                return position.name
                    ? `    at ${position.name} (${location})`
                    : `    at ${location}`
            } catch {
                return line
            }
        })
        return {
            stackTrace: translated ? result.join('\n') : stackTrace,
            didTranslate: translated,
        }
    } finally {
        for (const entry of leases.values()) entry.users -= 1
        trimMaps()
    }
}
