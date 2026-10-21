import { describe, expect, it, vi } from 'vitest'

import {
    beginCasJob,
    createNativeDurableAssetWriteSessionFactory,
    createNativeDurableCasJobSessionFactory,
    createNativeCasReadBudget,
    createNativeCasUploadBudget,
    createNativeImmutablePayloadCas,
    createNativeRemoteAssetReader,
    createNativeNewInlayImageEncoder,
    finalizeContentCasJob,
    NATIVE_CAS_READ_PART_BYTES,
    NativeCasReadStallError,
    pinExistingCasObject,
    prepareCasObject,
    releaseCasJob,
    sealCasJob,
} from './nativeAssetRepository'
import { hashPayloadBytes, objectPhysicalKey, PayloadSizeMismatchError } from './payloadCas'
import type { NativeByteInvoke } from './nativeAssetByteTransport'

const P = NATIVE_CAS_READ_PART_BYTES
const objectHash = '11'.repeat(32)

it('returns ordered bounded native presence batches and rejects malformed size responses', async () => {
    const hashes = Array.from({ length: 1030 }, (_, index) => index.toString(16).padStart(64, '0'))
    const invoke = vi.fn(async (_command: string, args: unknown) => {
        const page = (args as { contentHashes: string[] }).contentHashes
        if (page.length > 512) throw new Error('Oversized presence request')
        return page.map(hash => Number.parseInt(hash, 16) % 2 ? 4 : null)
    })
    const cas = createNativeImmutablePayloadCas(invoke)
    const remote = createNativeRemoteAssetReader(invoke)
    const expected = hashes.map((_hash, index) => index % 2 ? 4 : null)
    expect(await cas.statObjects!(hashes)).toEqual(expected)
    expect(await remote.statObjects!(hashes)).toEqual(expected)
    invoke.mockResolvedValueOnce([])
    await expect(cas.statObjects!([objectHash])).rejects.toThrow('response mismatch')
})

function sameBytes(actual: Uint8Array | null, expected: Uint8Array): boolean {
    return actual !== null && Buffer.from(actual).equals(Buffer.from(expected))
}

/** Serves one stored object the way `asset_cas_read_bytes` answers, including its input checks. */
function servedObject(source: Uint8Array | null) {
    return vi.fn(async (command: string, args?: Record<string, unknown>) => {
        if (command === 'asset_cas_stat_object') return source?.length ?? null
        if (command !== 'asset_cas_read_bytes') throw new Error(`Unexpected command ${command}`)
        const start = Number(args?.start)
        const end = Number(args?.endExclusive)
        if (end < start || end - start > P || end > Number(args?.expectedSize)) throw 'invalid CAS read range'
        if (source === null) throw 'cas-object-missing'
        if (source.length !== args?.expectedSize) throw 'cas-object-size-mismatch'
        return source.slice(start, end).buffer
    })
}

function readCommands(invoke: ReturnType<typeof vi.fn>) {
    return invoke.mock.calls.filter(([command]) => command === 'asset_cas_read_bytes')
}

describe('native CAS reads', () => {
    it('reads an object of at most one part in one call without a stat when its size is known', async () => {
        const source = Uint8Array.from({ length: 100 * 1024 }, (_, index) => index % 251)
        const invoke = servedObject(source)
        const cas = createNativeImmutablePayloadCas(invoke, createNativeCasReadBudget())

        await expect(cas.prepare(Uint8Array.of(1, 2, 3))).rejects.toThrow('durable ownership session')
        expect(sameBytes(await cas.readObject(objectHash, source.length), source)).toBe(true)
        expect(invoke.mock.calls).toEqual([['asset_cas_read_bytes', {
            contentHash: objectHash, start: 0, endExclusive: source.length, expectedSize: source.length,
        }]])

        invoke.mockClear()
        expect(sameBytes(await cas.readObject(objectHash), source)).toBe(true)
        expect(invoke.mock.calls.map(([command]) => command)).toEqual(['asset_cas_stat_object', 'asset_cas_read_bytes'])
        await expect(cas.statObject(objectHash)).resolves.toBe(source.length)
    })

    it.each([0, 1, 64 * 1024 - 1, 64 * 1024, 64 * 1024 + 1, 256 * 1024 - 1, 256 * 1024, 256 * 1024 + 1, P - 1, P, P + 1, 3 * P + 5])(
        'reads a %i-byte object exactly, one call per part',
        async (size) => {
            const source = Uint8Array.from({ length: size }, (_, index) => (index * 7) % 251)
            const invoke = servedObject(source)
            const result = await createNativeImmutablePayloadCas(invoke, createNativeCasReadBudget()).readObject(objectHash, size)
            expect(sameBytes(result, source)).toBe(true)
            expect(readCommands(invoke)).toHaveLength(Math.max(1, Math.ceil(size / P)))
        },
    )

    it('reads ranges that cross parts, end at EOF or start beyond it', async () => {
        const size = 3 * P + 5
        const source = Uint8Array.from({ length: size }, (_, index) => (index * 13) % 251)
        const cas = createNativeImmutablePayloadCas(servedObject(source), createNativeCasReadBudget())
        for (const [start, end] of [
            [P - 3, P + 3], [P - 1, 2 * P + 1], [5, 5], [size - 10, size], [size - 10, size + 99],
            [0, Number.MAX_SAFE_INTEGER], [size, size + 1], [size + 1, Number.MAX_SAFE_INTEGER],
        ]) {
            const result = await cas.readObjectRange(objectHash, { start, endExclusive: end }, size)
            expect(sameBytes(result, source.subarray(Math.min(start, size), Math.min(end, size)))).toBe(true)
        }
        await expect(cas.readObjectRange(objectHash, { start: 2, endExclusive: 1 }, size)).rejects.toThrow(RangeError)
    })

    it('returns null for a missing object, even for an empty range, and empty bytes for an empty object', async () => {
        const missing = createNativeImmutablePayloadCas(servedObject(null), createNativeCasReadBudget())
        await expect(missing.readObject(objectHash, 10)).resolves.toBeNull()
        await expect(missing.readObject(objectHash)).resolves.toBeNull()
        await expect(missing.readObjectRange(objectHash, { start: 0, endExclusive: 0 }, 10)).resolves.toBeNull()
        await expect(missing.readObjectRange(objectHash, { start: 20, endExclusive: 30 }, 10)).resolves.toBeNull()

        const empty = createNativeImmutablePayloadCas(servedObject(new Uint8Array()), createNativeCasReadBudget())
        await expect(empty.readObject(objectHash, 0)).resolves.toEqual(new Uint8Array())
        await expect(empty.readObject(objectHash)).resolves.toEqual(new Uint8Array())
    })

    it('reports a size mismatch and never reserves a result before the first part confirms the object', async () => {
        const budget = createNativeCasReadBudget()
        const changed = createNativeImmutablePayloadCas(servedObject(new Uint8Array(200)), budget)
        await expect(changed.readObjectRange(objectHash, { start: 0, endExclusive: 10 }, 100))
            .rejects.toBeInstanceOf(PayloadSizeMismatchError)
        // A result of this size cannot be allocated, so only a deferred allocation reaches native.
        const huge = Number.MAX_SAFE_INTEGER
        await expect(createNativeImmutablePayloadCas(servedObject(null), budget).readObject(objectHash, huge)).resolves.toBeNull()
        await expect(changed.readObject(objectHash, huge)).rejects.toBeInstanceOf(PayloadSizeMismatchError)
        await expect(changed.readObject(objectHash, -1)).rejects.toThrow(TypeError)
    })

    it.each(['missing', 'changed', 'failed', 'short'] as const)('settles a read above one part when a later part is %s', async (outcome) => {
        const size = 2 * P + 1
        const invoke = vi.fn(async (_command: string, args?: Record<string, unknown>) => {
            const start = Number(args?.start)
            const end = Number(args?.endExclusive)
            if (start > 0) {
                if (outcome === 'missing') throw 'cas-object-missing'
                if (outcome === 'changed') throw 'cas-object-size-mismatch'
                if (outcome === 'failed') throw 'persistent storage is unavailable during device backup maintenance'
                return new Uint8Array(end - start - 1).buffer
            }
            return new Uint8Array(end - start).buffer
        })
        const result = createNativeImmutablePayloadCas(invoke, createNativeCasReadBudget()).readObject(objectHash, size)
        if (outcome === 'missing') await expect(result).resolves.toBeNull()
        else if (outcome === 'changed') await expect(result).rejects.toBeInstanceOf(PayloadSizeMismatchError)
        else if (outcome === 'failed') await expect(result).rejects.toBe('persistent storage is unavailable during device backup maintenance')
        else await expect(result).rejects.toThrow('invalid length')
        expect(readCommands(invoke)).toHaveLength(2)
    })

    it('completes 64 concurrent reads above and below one part within the document budget', async () => {
        const sources = new Map(Array.from({ length: 64 }, (_, index) => [
            index.toString(16).padStart(2, '0').repeat(32),
            new Uint8Array(index % 2 ? P + 3 + index : 100 + index).fill(index),
        ]))
        let inFlight = 0
        let peak = 0
        const invoke = vi.fn(async (_command: string, args?: Record<string, unknown>) => {
            const length = Number(args?.endExclusive) - Number(args?.start)
            const weight = Math.max(length, 256 * 1024)
            inFlight += weight
            peak = Math.max(peak, inFlight)
            await new Promise((resolve) => setTimeout(resolve, 0))
            inFlight -= weight
            const source = sources.get(String(args?.contentHash))!
            return source.slice(Number(args?.start), Number(args?.endExclusive)).buffer
        })
        const cas = createNativeImmutablePayloadCas(invoke, createNativeCasReadBudget())
        const results = await Promise.all([...sources].map(([hash, source]) => cas.readObject(hash, source.length)))
        expect(results.every((result, index) => sameBytes(result, [...sources.values()][index]))).toBe(true)
        expect(peak).toBeGreaterThan(P)
        expect(peak).toBeLessThanOrEqual(8 * 1024 * 1024)
    })

    it('shares one default budget between CAS and remote reads and sends nothing beyond it until an invoke settles', async () => {
        const held: (() => void)[] = []
        const invoke = vi.fn((command: string, args?: Record<string, unknown>) => {
            if (command === 'asset_remote_hydrate_object') return Promise.resolve(P)
            const length = Number(args?.endExclusive) - Number(args?.start)
            return new Promise((resolve) => held.push(() => resolve(new Uint8Array(length).buffer)))
        })
        const reads = [
            ...Array.from({ length: 6 }, () => createNativeImmutablePayloadCas(invoke).readObject(objectHash, P)),
            ...Array.from({ length: 6 }, () => createNativeRemoteAssetReader(invoke).readObject(objectHash)),
        ]
        await vi.waitFor(() => expect(readCommands(invoke)).toHaveLength(8))
        await new Promise((resolve) => setTimeout(resolve, 10))
        expect(readCommands(invoke)).toHaveLength(8)
        held.shift()!()
        await vi.waitFor(() => expect(readCommands(invoke)).toHaveLength(9))
        while (held.length > 0 || readCommands(invoke).length < 12) {
            if (held.length > 0) held.shift()!()
            await new Promise((resolve) => setTimeout(resolve, 0))
        }
        expect((await Promise.all(reads)).every((result) => result?.byteLength === P)).toBe(true)
    })

    it('serves waiting reads in order and releases a stalled invoke once, discarding its late result', async () => {
        vi.useFakeTimers()
        try {
            const held: ((value: unknown) => void)[] = []
            const invoke = vi.fn(() => new Promise((resolve) => held.push(resolve)))
            const budget = createNativeCasReadBudget({ capacityBytes: 2, minimumWeightBytes: 1, stallMs: 1000 })
            const cas = createNativeImmutablePayloadCas(invoke, budget)
            const stalled = cas.readObject(objectHash, 1)
            const large = cas.readObject(objectHash, 2)
            const small = cas.readObject(objectHash, 1)
            await vi.advanceTimersByTimeAsync(999)
            expect(readCommands(invoke)).toHaveLength(1)
            const failed = expect(stalled).rejects.toBeInstanceOf(NativeCasReadStallError)
            await vi.advanceTimersByTimeAsync(1)
            await failed
            expect(readCommands(invoke)).toHaveLength(2)
            held[0](Uint8Array.of(9).buffer)
            await vi.advanceTimersByTimeAsync(0)
            expect(readCommands(invoke)).toHaveLength(2)
            held[1](Uint8Array.of(1, 2).buffer)
            await expect(large).resolves.toEqual(Uint8Array.of(1, 2))
            await vi.advanceTimersByTimeAsync(0)
            held[2](Uint8Array.of(3).buffer)
            await expect(small).resolves.toEqual(Uint8Array.of(3))
        } finally {
            vi.useRealTimers()
        }
    })

    it('reads hydrated remote bytes at the hydrated size and treats a vanished object as missing', async () => {
        const source = Uint8Array.from({ length: P + 200 }, (_, index) => index % 251)
        let present = true
        const served = servedObject(source)
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'asset_remote_hydrate_object') return source.length
            if (!present && Number(args?.start) > 0) throw 'cas-object-missing'
            return served(command, args)
        })
        const remote = createNativeRemoteAssetReader(invoke, createNativeCasReadBudget())
        expect(sameBytes(await remote.readObject(objectHash), source)).toBe(true)
        expect(invoke.mock.calls.filter(([command]) => command === 'asset_remote_hydrate_object')).toHaveLength(1)
        expect(readCommands(invoke)).toHaveLength(2)
        expect(sameBytes(await remote.readObject(objectHash, { start: P - 1, endExclusive: P + 999 }), source.subarray(P - 1))).toBe(true)
        present = false
        await expect(remote.readObject(objectHash)).resolves.toBeNull()
    })
})

describe('native asset repository adapters', () => {
    function uploadBoundary(options: {
        maxUploads?: number
        capacityBytes?: number
        beforeChunk?: () => Promise<void>
        beforeCancel?: () => Promise<void>
        failCommand?: string
    } = {}) {
        const active = new Map<string, { data: Uint8Array, offset: number }>()
        const stored = new Map<string, Uint8Array>()
        let reservedBytes = 0
        let peakUploads = 0
        let peakBytes = 0
        const save = async (data: Uint8Array) => {
            const contentHash = await hashPayloadBytes(data)
            stored.set(contentHash, data.slice())
            return { contentHash, byteSize: data.length, physicalKey: objectPhysicalKey(contentHash), deduplicated: false }
        }
        const cancel = (id: string) => {
            const upload = active.get(id)
            if (upload) reservedBytes -= upload.data.length
            active.delete(id)
        }
        const invoke = vi.fn<NativeByteInvoke>(async (command, args) => {
            if (command === options.failCommand) throw new Error('Synthetic transfer failure')
            if (command === 'native_asset_bytes_probe') return false
            if (command === 'asset_cas_job_begin') return crypto.randomUUID()
            const input = args as Record<string, unknown>
            if (command === 'asset_cas_job_prepare') return save(Uint8Array.from(input.data as number[]))
            const id = String(input.uploadId)
            if (command === 'asset_cas_job_upload_open') {
                const size = Number(input.totalBytes)
                if (active.size >= (options.maxUploads ?? 4)) throw new Error('CAS upload capacity is unavailable')
                if (reservedBytes + size > (options.capacityBytes ?? 1024 * 1024 * 1024)) throw new Error('CAS upload byte quota is unavailable')
                active.set(id, { data: new Uint8Array(size), offset: 0 })
                reservedBytes += size
                peakUploads = Math.max(peakUploads, active.size)
                peakBytes = Math.max(peakBytes, reservedBytes)
                return { capacity: 64 * 1024 }
            }
            if (command === 'asset_cas_job_upload_cancel') {
                await options.beforeCancel?.()
                cancel(id)
                return
            }
            const upload = active.get(id)
            if (!upload) throw new Error('CAS upload is missing')
            if (command === 'asset_cas_job_upload_chunk') {
                await options.beforeChunk?.()
                const data = Uint8Array.from(input.data as number[])
                if (input.offset !== upload.offset || data.length > 64 * 1024) throw new Error('Invalid CAS upload chunk')
                upload.data.set(data, upload.offset)
                upload.offset += data.length
                return upload.offset
            }
            if (command === 'asset_cas_job_upload_finish') {
                if (upload.offset !== upload.data.length) throw new Error('CAS upload is incomplete')
                cancel(id)
                return save(upload.data)
            }
            throw new Error(`Unexpected command ${command}`)
        })
        return { invoke, stored, active, usage: () => ({ reservedBytes, peakUploads, peakBytes }) }
    }

    it('stores ten concurrent large assets across ownership sessions without exceeding native upload admission', async () => {
        let resume!: () => void
        const held = new Promise<void>(resolve => { resume = resolve })
        const native = uploadBoundary({ beforeChunk: () => held })
        const factories = [
            createNativeDurableAssetWriteSessionFactory(native.invoke),
            createNativeDurableCasJobSessionFactory('card-or-module-content-import', native.invoke),
        ]
        const sources = Array.from({ length: 10 }, (_, index) => new Uint8Array(64 * 1024 + 1).fill(index))
        const sessions = await Promise.all(sources.map((_, index) => factories[index % factories.length].begin()))
        const writing = Promise.allSettled(sessions.map((session, index) => session.prepare(sources[index])))
        await vi.waitFor(() => expect(native.active.size).toBe(4)).finally(resume)
        const outcomes = await writing
        expect(outcomes.map(outcome => outcome.status)).toEqual(sources.map(() => 'fulfilled'))
        for (const [index, outcome] of outcomes.entries()) {
            if (outcome.status !== 'fulfilled') throw outcome.reason
            expect(outcome.value.byteSize).toBe(sources[index].length)
            expect(native.stored.get(outcome.value.contentHash)).toEqual(sources[index])
        }
        expect(native.usage().peakUploads).toBe(4)
        expect(native.active.size).toBe(0)
        expect(native.usage().reservedBytes).toBe(0)
    })

    it('waits for byte quota even when native upload slots remain available', async () => {
        const size = 64 * 1024 + 1
        const capacityBytes = 2 * size
        const budget = createNativeCasUploadBudget({ capacityBytes })
        let resume!: () => void
        const held = new Promise<void>(resolve => { resume = resolve })
        const native = uploadBoundary({ capacityBytes, beforeChunk: () => held })
        const sources = Array.from({ length: 3 }, (_, index) => new Uint8Array(size).fill(index))
        const writing = Promise.allSettled(sources.map(data =>
            prepareCasObject('byte-quota', data, 'direct-object', native.invoke, budget)))
        await vi.waitFor(() => expect(native.active.size).toBe(2)).finally(resume)
        const outcomes = await writing
        expect(outcomes.map(outcome => outcome.status)).toEqual(['fulfilled', 'fulfilled', 'fulfilled'])
        for (const [index, outcome] of outcomes.entries()) {
            if (outcome.status !== 'fulfilled') throw outcome.reason
            expect(native.stored.get(outcome.value.contentHash)).toEqual(sources[index])
        }
        expect(native.usage()).toEqual({ reservedBytes: 0, peakUploads: 2, peakBytes: capacityBytes })
    })

    it.each([
        'native_asset_bytes_probe',
        'asset_cas_job_upload_open',
        'asset_cas_job_upload_chunk',
        'asset_cas_job_upload_finish',
    ])('allows the next upload after %s fails', async failCommand => {
        const budget = createNativeCasUploadBudget({ maxUploads: 1 })
        const failing = uploadBoundary({ failCommand })
        const next = uploadBoundary()
        const data = new Uint8Array(64 * 1024 + 1).fill(7)
        const outcomes = await Promise.allSettled([
            prepareCasObject('failed-transfer', data, 'direct-object', failing.invoke, budget),
            prepareCasObject('next-transfer', data, 'direct-object', next.invoke, budget),
        ])
        expect(outcomes[0]).toMatchObject({ status: 'rejected', reason: new Error('Synthetic transfer failure') })
        const saved = outcomes[1]
        if (saved.status !== 'fulfilled') throw saved.reason
        expect(next.stored.get(saved.value.contentHash)).toEqual(data)
        expect(failing.active.size).toBe(0)
        expect(failing.usage().reservedBytes).toBe(0)
        expect(next.active.size).toBe(0)
    })

    it('holds admission until a failed upload finishes its cancellation cleanup', async () => {
        const budget = createNativeCasUploadBudget({ maxUploads: 1 })
        let resume!: () => void
        let cancelling = false
        const held = new Promise<void>(resolve => { resume = resolve })
        const failing = uploadBoundary({
            failCommand: 'asset_cas_job_upload_chunk',
            beforeCancel: () => { cancelling = true; return held },
        })
        const next = uploadBoundary()
        const data = new Uint8Array(64 * 1024 + 1)
        const writing = Promise.allSettled([
            prepareCasObject('cancelled-transfer', data, 'direct-object', failing.invoke, budget),
            prepareCasObject('queued-transfer', data, 'direct-object', next.invoke, budget),
        ])
        try {
            await vi.waitFor(() => expect(cancelling).toBe(true))
            expect(failing.active.size).toBe(1)
            expect(next.invoke).not.toHaveBeenCalled()
        } finally {
            resume()
        }
        expect((await writing).map(outcome => outcome.status)).toEqual(['rejected', 'fulfilled'])
        expect(failing.active.size).toBe(0)
        expect(next.active.size).toBe(0)
    })

    it('keeps a small direct write available while streamed uploads wait for admission', async () => {
        const budget = createNativeCasUploadBudget({ maxUploads: 1 })
        let resume!: () => void
        const held = new Promise<void>(resolve => { resume = resolve })
        const native = uploadBoundary({ beforeChunk: () => held })
        const large = prepareCasObject('large-transfer', new Uint8Array(64 * 1024 + 1), 'direct-object', native.invoke, budget)
        try {
            await vi.waitFor(() => expect(native.active.size).toBe(1))
            const data = Uint8Array.of(1, 2, 3)
            const saved = await prepareCasObject('small-transfer', data, 'direct-object', native.invoke, budget)
            expect(native.stored.get(saved.contentHash)).toEqual(data)
        } finally {
            resume()
            await large
        }
    })

    it('admits queued uploads in arrival order when a smaller request could fit first', async () => {
        const budget = createNativeCasUploadBudget({ maxUploads: 2, capacityBytes: 3 })
        const started: string[] = []
        let resume!: () => void
        const held = new Promise<void>(resolve => { resume = resolve })
        const writing = Promise.all([
            budget.run(2, async () => { started.push('first'); await held; return 'first' }),
            budget.run(2, async () => { started.push('second'); return 'second' }),
            budget.run(1, async () => { started.push('third'); return 'third' }),
        ])
        await vi.waitFor(() => expect(started).toEqual(['first'])).finally(resume)
        expect(await writing).toEqual(['first', 'second', 'third'])
        expect(started).toEqual(['first', 'second', 'third'])
    })

    it('rejects uploads that cannot fit instead of blocking later requests', async () => {
        const budget = createNativeCasUploadBudget({ capacityBytes: 2 })
        await expect(budget.run(3, async () => 'too large')).rejects.toThrow(RangeError)
        await expect(budget.run(2, async () => 'fits')).resolves.toBe('fits')
        await expect(createNativeCasUploadBudget().run(512 * 1024 * 1024 + 1, async () => 'too large'))
            .rejects.toThrow(RangeError)
        expect(() => createNativeCasUploadBudget({ maxUploads: 5 })).toThrow(RangeError)
        expect(() => createNativeCasUploadBudget({ capacityBytes: 1024 * 1024 * 1024 + 1 })).toThrow(RangeError)
    })

    it('streams large CAS writes through bounded chunks and always cancels the spool', async () => {
        const hash = '22'.repeat(32)
        const acknowledgements: number[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'native_asset_bytes_probe') return false
            if (command === 'asset_cas_job_upload_open') {
                return { capacity: 64 * 1024 }
            }
            if (command === 'asset_cas_job_upload_chunk') {
                const next = Number(args?.offset) + (args?.data as number[]).length
                acknowledgements.push(next)
                return next
            }
            if (command === 'asset_cas_job_upload_finish') {
                return {
                    contentHash: hash,
                    byteSize: 64 * 1024 + 3,
                    physicalKey: `assets/objects/22/${hash.slice(2)}`,
                    deduplicated: false,
                }
            }
            return undefined
        })

        await expect(prepareCasObject(
            'session-large',
            new Uint8Array(64 * 1024 + 3),
            'direct-object',
            invoke,
        )).resolves.toMatchObject({ byteSize: 64 * 1024 + 3 })

        expect(acknowledgements).toEqual([64 * 1024, 64 * 1024 + 3])
        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'native_asset_bytes_probe',
            'asset_cas_job_upload_open',
            'asset_cas_job_upload_chunk',
            'asset_cas_job_upload_chunk',
            'asset_cas_job_upload_finish',
            'asset_cas_job_upload_cancel',
        ])
        expect(invoke).not.toHaveBeenCalledWith('asset_cas_job_prepare', expect.anything())
        const chunks = invoke.mock.calls
            .filter(([command]) => command === 'asset_cas_job_upload_chunk')
            .map(([, args]) => (args?.data as number[]).length)
        expect(chunks).toEqual([64 * 1024, 3])
    })

    it('forwards configurable inlay options and accepts truthful PNG metadata', async () => {
        const invoke = vi.fn(async () => ({
            data: [4, 5, 6],
            outputSize: 3,
            metadata: {
                key: 'inlay-id',
                kind: 'inlay',
                size: 3,
                mime: 'image/png',
                name: 'Image',
                ext: 'png',
                inlayType: 'image',
                width: 13,
                height: 17,
            },
        }))
        const encoder = createNativeNewInlayImageEncoder(invoke)

        await expect(encoder.encodeNewInlayImage(
            'inlay-id',
            Uint8Array.of(1, 2),
            { name: 'Image', options: { format: 'png', quality: 12, maxDimension: 256, skipReencode: true, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0 } },
        )).resolves.toEqual({
            data: Uint8Array.of(4, 5, 6),
            metadata: {
                kind: 'inlay',
                mime: 'image/png',
                name: 'Image',
                ext: 'png',
                inlayType: 'image',
                width: 13,
                height: 17,
            },
        })
        expect(invoke).toHaveBeenCalledOnce()
        expect(invoke).toHaveBeenCalledWith('native_media_encode_inlay_image', {
            id: 'inlay-id', data: [1, 2], name: 'Image',
            options: { format: 'png', quality: 12, maxDimension: 256, skipReencode: true, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0 },
        })
    })

    it('streams large Inlay encoder input and output through bounded native media commands', async () => {
        const transferSize = 64 * 1024 + 1
        const source = new Uint8Array(transferSize)
        source.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
        const output = new Uint8Array(transferSize)
        output[transferSize - 1] = 7
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'native_asset_bytes_probe') return false
            if (command === 'native_media_inlay_input_open') return { capacity: 64 * 1024 }
            if (command === 'native_media_inlay_input_chunk') {
                return Number(args?.offset) + (args?.data as number[]).length
            }
            if (command === 'native_media_encode_inlay_finish') {
                return {
                    data: null,
                    outputId: 'encoded-output',
                    outputSize: transferSize,
                    metadata: {
                        key: 'large-inlay', kind: 'inlay', size: transferSize,
                        mime: 'image/png', name: 'large.png', ext: 'png',
                        inlayType: 'image', width: 1, height: 1,
                    },
                }
            }
            if (command === 'native_media_inlay_output_read') {
                return Array.from(output.subarray(
                    Number(args?.start),
                    Number(args?.endExclusive),
                ))
            }
            return undefined
        })

        await expect(createNativeNewInlayImageEncoder(invoke).encodeNewInlayImage(
            'large-inlay',
            source,
            { name: 'large.png', options: { format: 'png', quality: 85, maxDimension: 0, skipReencode: false, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0 } },
        )).resolves.toMatchObject({ data: output })

        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'native_asset_bytes_probe',
            'native_media_inlay_input_open',
            'native_media_inlay_input_chunk',
            'native_media_inlay_input_chunk',
            'native_media_encode_inlay_finish',
            'native_media_inlay_input_cancel',
            'native_media_inlay_output_read',
            'native_media_inlay_output_read',
            'native_media_inlay_output_cancel',
        ])
        expect(invoke).not.toHaveBeenCalledWith(
            'native_media_encode_inlay_image',
            expect.anything(),
        )
    })

    it('keeps a large source the native encoder left unchanged without downloading it back', async () => {
        const transferSize = 64 * 1024 + 1
        const source = new Uint8Array(transferSize)
        source.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            if (command === 'native_asset_bytes_probe') return false
            if (command === 'native_media_inlay_input_open') return { capacity: 64 * 1024 }
            if (command === 'native_media_inlay_input_chunk') {
                return Number(args?.offset) + (args?.data as number[]).length
            }
            if (command === 'native_media_encode_inlay_finish') {
                return {
                    data: null,
                    outputId: null,
                    outputSize: transferSize,
                    sourceKept: true,
                    metadata: {
                        key: 'kept-inlay', kind: 'inlay', size: transferSize,
                        mime: 'image/png', name: 'kept.png', ext: 'png',
                        inlayType: 'image', width: 1, height: 1,
                        preservationReason: 'color-profile',
                    },
                }
            }
            return undefined
        })

        const encoded = await createNativeNewInlayImageEncoder(invoke).encodeNewInlayImage(
            'kept-inlay',
            source,
            { name: 'kept.png', options: { format: 'webp', quality: 85, maxDimension: 0, skipReencode: true, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0 } },
        )

        expect(encoded.data).toBe(source)
        expect(encoded.metadata).toMatchObject({ mime: 'image/png', ext: 'png', preservationReason: 'color-profile' })
        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'native_asset_bytes_probe',
            'native_media_inlay_input_open',
            'native_media_inlay_input_chunk',
            'native_media_inlay_input_chunk',
            'native_media_encode_inlay_finish',
            'native_media_inlay_input_cancel',
        ])
    })

    it('normalizes ignored original conversion options before invoking native code', async () => {
        const source = Uint8Array.of(0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a)
        const invoke = vi.fn(async () => ({
            data: Array.from(source),
            outputSize: source.byteLength,
            metadata: {
                key: 'original-id', kind: 'inlay', size: source.byteLength,
                mime: 'image/png', name: 'original.png', ext: 'png', inlayType: 'image',
                width: 1, height: 1,
            },
        }))
        const encoder = createNativeNewInlayImageEncoder(invoke)

        await encoder.encodeNewInlayImage('original-id', source, {
            name: 'original.png',
            options: {
                format: 'original', quality: Number.NaN,
                maxDimension: Number.MAX_SAFE_INTEGER, skipReencode: true, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0,
            },
        })

        expect(invoke).toHaveBeenCalledWith('native_media_encode_inlay_image', {
            id: 'original-id', data: Array.from(source), name: 'original.png',
            options: {
                format: 'original', quality: 85, maxDimension: 4_294_967_295,
                skipReencode: true, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0,
            },
        })
    })

    it.each([
        ['webp', 'image/png', 'png'],
        ['png', 'image/webp', 'webp'],
        ['original', 'image/webp', 'png'],
    ] as const)('rejects a native %s response that is neither the requested format nor the original image', async (format, mime, ext) => {
        const encoder = createNativeNewInlayImageEncoder(async () => ({
            data: [4],
            outputSize: 1,
            metadata: {
                key: 'inlay-id', kind: 'inlay', size: 1, mime, name: 'Image', ext,
                inlayType: 'image', width: 1, height: 1,
            },
        }))

        await expect(encoder.encodeNewInlayImage('inlay-id', Uint8Array.of(1, 2, 3), {
            name: 'Image', options: { format, quality: 85, maxDimension: 0, skipReencode: false, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0 },
        })).rejects.toThrow('neither the requested format nor the original image')
    })

    it('accepts a native response that kept the original image, with or without its size', async () => {
        const source = Uint8Array.of(0x47, 0x49, 0x46, 0x38)
        const encoder = createNativeNewInlayImageEncoder(async () => ({
            data: Array.from(source),
            outputSize: source.byteLength,
            metadata: {
                key: 'kept', kind: 'inlay', size: source.byteLength, mime: 'image/gif',
                name: 'loop.gif', ext: 'gif', inlayType: 'image',
            },
        }))

        await expect(encoder.encodeNewInlayImage('kept', source, {
            name: 'loop.gif', options: { format: 'webp', quality: 85, maxDimension: 0, skipReencode: false, animationDecodeBytes: 256 * 1024 * 1024, animationMaxFps: 0 },
        })).resolves.toEqual({
            data: source,
            metadata: {
                kind: 'inlay', mime: 'image/gif', name: 'loop.gif', ext: 'gif',
                inlayType: 'image', width: undefined, height: undefined,
            },
        })
    })

    it('exposes a native-only durable CAS pin session without catalog enumeration', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_job_begin') return 'session-1'
            if (command === 'asset_cas_job_prepare') return {
                contentHash: '44'.repeat(32),
                byteSize: 3,
                physicalKey: `assets/objects/44/${'44'.repeat(32).slice(2)}`,
                deduplicated: false,
                directoryEntriesSynced: false,
            }
            return null
        })

        await expect(beginCasJob('lossless-import', invoke)).resolves.toBe('session-1')
        await expect(prepareCasObject(
            'session-1',
            Uint8Array.of(1, 2, 3),
            'direct-object',
            invoke,
        )).resolves.toMatchObject({ byteSize: 3 })
        await pinExistingCasObject('session-1', '55'.repeat(32), 7, 'owner-manifest', invoke)
        await sealCasJob('session-1', invoke)
        await releaseCasJob('session-1', 'committed', invoke)

        expect(invoke.mock.calls.map(([command]) => command)).toEqual([
            'asset_cas_job_begin',
            'asset_cas_job_prepare',
            'asset_cas_job_pin_existing',
            'asset_cas_job_seal',
            'asset_cas_job_release',
        ])
        expect(invoke).not.toHaveBeenCalledWith(expect.stringContaining('catalog'), expect.anything())
    })

    it('binds normal asset publication to one direct-write native session', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_job_begin') return 'asset-write-1'
            if (command === 'asset_cas_job_prepare') return {
                contentHash: '66'.repeat(32),
                byteSize: 2,
                physicalKey: `assets/objects/66/${'66'.repeat(32).slice(2)}`,
                deduplicated: true,
            }
            return null
        })
        const factory = createNativeDurableAssetWriteSessionFactory(invoke)

        const session = await factory.begin()
        await session.prepare(Uint8Array.of(9, 8))
        await session.seal()
        await session.release('committed')

        expect(invoke.mock.calls).toEqual([
            ['asset_cas_job_begin', { kind: 'direct-asset-or-inlay-write' }],
            ['asset_cas_job_prepare', {
                sessionId: 'asset-write-1',
                data: [9, 8],
                role: 'direct-object',
            }],
            ['asset_cas_job_seal', { sessionId: 'asset-write-1' }],
            ['asset_cas_job_release', {
                sessionId: 'asset-write-1',
                outcome: 'committed',
            }],
        ])
    })

    it('labels direct cold ownership with its exact durable job kind', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'asset_cas_job_begin') return 'cold-write-1'
            if (command === 'asset_cas_job_prepare') return {
                contentHash: '77'.repeat(32),
                byteSize: 1,
                physicalKey: `assets/objects/77/${'77'.repeat(32).slice(2)}`,
                deduplicated: false,
            }
            return null
        })
        expect(invoke.mock.calls.map(([command]) => command)).not.toContain('asset_cas_prepare')
        const session = await createNativeDurableCasJobSessionFactory(
            'cold-direct-write',
            invoke,
        ).begin()

        await session.prepare(Uint8Array.of(7))
        await session.seal()
        await session.release('committed')

        expect(invoke.mock.calls[0]).toEqual([
            'asset_cas_job_begin',
            { kind: 'cold-direct-write' },
        ])
    })

    it('finalizes one content session without echoing asset descriptors through IPC', async () => {
        const manifestHash = '66'.repeat(32)
        const invoke = vi.fn(async () => ({
            contentHash: manifestHash,
            byteSize: 3,
            physicalKey: `assets/objects/66/${manifestHash.slice(2)}`,
            deduplicated: false,
        }))

        await expect(finalizeContentCasJob(
            'content-1',
            Uint8Array.of(1, 2, 3),
            invoke,
        )).resolves.toMatchObject({ contentHash: manifestHash, byteSize: 3 })

        expect(invoke).toHaveBeenCalledOnce()
        expect(invoke).toHaveBeenCalledWith('asset_cas_job_finalize_content', {
            sessionId: 'content-1',
            ownerManifest: [1, 2, 3],
        })
        expect(JSON.stringify(invoke.mock.calls)).not.toContain('contentAssets')
    })
})
