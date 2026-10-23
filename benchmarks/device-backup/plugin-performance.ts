import { SafeLocalPluginStorage } from '../../src/ts/plugins/pluginSafeClass'
import { PluginDeviceKeyspace, createNativePluginDeviceBackend } from '../../src/ts/plugins/pluginDeviceKeyspace'

// Standalone consumer of the production wrapper, IPC transport and SQLite store.
// Both revisions run this identical module; fixture construction and validation
// stay outside timed operations. A sample is the mean of sequential operations.
export function pluginPerformance(ready: () => Promise<void>) {
    const results: Record<string, unknown>[] = []
    let phase = 'idle'
    let failure: string | undefined
    const fixture = (count: number) => Array.from({ length: count }, (_, i) => ({
        id: i, name: `entry-${i}`, enabled: i % 2 === 0, score: i / 7,
    }))
    const specs = [
        { name: 'small-json', make: () => ({ enabled: true, name: 'plugin', count: 42, tags: ['a', 'b'] }), writes: 20, reads: 2000, cold: 20 },
        { name: 'array-1000', make: () => fixture(1000), writes: 8, reads: 30, cold: 8 },
        { name: 'array-50000', make: () => fixture(50000), writes: 2, reads: 2, cold: 2 },
        { name: 'flat-json-8MiB', make: () => ({ payload: 'x'.repeat(8 * 1024 * 1024) }), writes: 1, reads: 1, cold: 1 },
    ]
    async function measure(name: string, iterations: number, operation: (index: number) => Promise<unknown>, before?: () => void) {
        phase = name
        const samplesMs: number[] = []
        // Two warm-up batches and seven observations, without forcing a GC in
        // each operation (natural GC pauses belong to the measured workload).
        for (let round = -2; round < 7; round++) {
            before?.()
            const started = performance.now()
            for (let i = 0; i < iterations; i++) await operation(i)
            if (round >= 0) samplesMs.push((performance.now() - started) / iterations)
        }
        const sorted = [...samplesMs].sort((a, b) => a - b)
        results.push({ name, iterations, samplesMs, medianMs: sorted[3], minMs: sorted[0], maxMs: sorted[6] })
    }
    async function jsonCases() {
        for (const spec of specs) {
            const owner = `synthetic-performance-${spec.name}`
            const keyspace = new PluginDeviceKeyspace(owner, createNativePluginDeviceBackend())
            const storage = new SafeLocalPluginStorage(owner, keyspace)
            const value = spec.make()
            const expected = JSON.stringify({ revision: 1, value })
            await storage.setItem('value', { revision: 1, value })
            if (JSON.stringify(await storage.getItem('value')) !== expected) throw new Error(`${spec.name}: JSON differs`)
            const start = results.length
            let revision = 1
            await measure(`${spec.name}/write`, spec.writes, () => storage.setItem('value', { revision: ++revision, value }))
            await storage.getItem('value')
            await measure(`${spec.name}/warm-read`, spec.reads, () => storage.getItem('value'))
            await measure(`${spec.name}/cold-read`, spec.cold, () => { keyspace.invalidate(); return storage.getItem('value') })
            for (const row of results.slice(start)) row.jsonBytes = new TextEncoder().encode(expected).byteLength
            const actual = await storage.getItem<{ revision: number; value: unknown }>('value')
            if (JSON.stringify(actual?.value) !== JSON.stringify(value)) throw new Error(`${spec.name}: final JSON differs`)
            await storage.clear()
            keyspace.dispose()
        }
    }
    async function binaryCases() {
        for (const size of [1, 8, 32]) {
            for (const type of ['ArrayBuffer', 'Blob'] as const) {
                const owner = `synthetic-performance-${type}-${size}`
                const keyspace = new PluginDeviceKeyspace(owner, createNativePluginDeviceBackend())
                const storage = new SafeLocalPluginStorage(owner, keyspace)
                const bytes = new Uint8Array(size * 1024 * 1024).map((_, i) => i % 251)
                const value = type === 'Blob' ? new Blob([bytes], { type: 'application/octet-stream' }) : bytes.buffer
                await storage.setItem('value', value)
                // Invalidate to verify the durable native path, not a held value.
                keyspace.invalidate()
                const actual = await storage.getItem('value')
                const preservedType = type === 'Blob' ? actual instanceof Blob : actual instanceof ArrayBuffer
                const restored = actual instanceof Blob ? new Uint8Array(await actual.arrayBuffer())
                    : actual instanceof ArrayBuffer ? new Uint8Array(actual) : null
                const preservedBytes = restored?.length === bytes.length && restored.every((byte, i) => byte === bytes[i])
                results.push({ name: `${type}-${size}MiB/correctness`, preservedType, preservedBytes: !!preservedBytes })
                if (preservedType && preservedBytes) {
                    await measure(`${type}-${size}MiB/write`, 1, () => storage.setItem('value', value))
                    await measure(`${type}-${size}MiB/warm-read`, 1, () => storage.getItem('value'))
                    await measure(`${type}-${size}MiB/cold-read`, 1, () => { keyspace.invalidate(); return storage.getItem('value') })
                }
                await storage.clear()
                keyspace.dispose()
            }
        }
        await mixedCase()
    }
    async function mixedCase() {
        // The same owner may keep small JSON settings beside binary content.
        const owner = 'synthetic-performance-mixed'
        const keyspace = new PluginDeviceKeyspace(owner, createNativePluginDeviceBackend())
        const storage = new SafeLocalPluginStorage(owner, keyspace)
        const settings = { enabled: true, count: 42 }
        await storage.setItem('settings', settings)
        await storage.setItem('binary', new Blob([new Uint8Array(1024 * 1024)]))
        keyspace.invalidate()
        await storage.getItem('settings')
        await measure('mixed-owner/small-json-warm-read', 100, () => storage.getItem('settings'))
        if (JSON.stringify(await storage.getItem('settings')) !== JSON.stringify(settings)) throw new Error('Mixed owner settings differ')
        await storage.clear()
        keyspace.dispose()
    }
    return {
        state: () => ({ phase, failure, results }),
        async start(options: { binary?: boolean } = {}) {
            if (phase !== 'idle') throw new Error('Fresh synthetic performance profile required')
            try {
                await ready()
                await jsonCases()
                if (options.binary === false) await mixedCase()
                else await binaryCases()
                phase = 'done'
            } catch (error) {
                failure = error instanceof Error ? `${error.message}\n${error.stack}` : String(error)
                phase = 'failed'
            }
        },
    }
}
