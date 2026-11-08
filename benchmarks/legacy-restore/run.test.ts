import { beforeEach, describe, expect, it, vi } from 'vitest'
// Match the WebView codec path instead of Node's internal Buffer API.
vi.hoisted(() => { vi.stubGlobal('Buffer', undefined) })
import { gunzipSync } from 'fflate'
import { unpack } from 'msgpackr/index-no-eval'
import { fixtureCharacter, planFixture } from './fixture'
import { runLegacyRestoreMeasurement, type ReadbackProgress } from './run'

const native = vi.hoisted(() => ({ invoke: vi.fn(), remove: vi.fn(), bytes: new Uint8Array(3_000_000), offset: 0, length: 0,
    retireActive: () => {}, activeIds: [] as string[], bufferPackets: false, nonPlainWrites: 0, corruptBinary: false }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))
vi.mock('@tauri-apps/api/path', () => ({ join: async (...parts: string[]) => parts.join('/') }))
vi.mock('../../src/ts/storage/nativePaths', () => ({ nativeDataPath: async () => '/synthetic' }))
vi.mock('@tauri-apps/plugin-fs', () => ({
    SeekMode: { Start: 0 }, mkdir: vi.fn(), remove: native.remove,
    open: async () => ({
        write: async (bytes: Uint8Array) => {
            if (Object.getPrototypeOf(bytes) !== Uint8Array.prototype) native.nonPlainWrites++
            // Short writes exercise the source writer's complete-write loop.
            const count = Math.min(bytes.length, 65_536)
            native.bytes.set(bytes.subarray(0, count), native.offset)
            native.offset += count
            native.length = Math.max(native.length, native.offset)
            return count
        },
        seek: async (offset: number) => { native.offset = offset }, close: async () => {},
    }),
}))
vi.mock('./fixture', async importOriginal => {
    const actual = await importOriginal<typeof import('./fixture')>()
    // Mirrors the WebView's Buffer polyfill, whose toJSON turns bytes into an object.
    class BufferLike extends Uint8Array { toJSON() { return { type: 'Buffer', data: [...this] } } }
    return { ...actual, planFixture: (_bytes: number, prefix?: string, binaryBytes?: number) =>
        actual.planFixture(2_500_000, prefix, binaryBytes),
        *fixturePackets(...args: Parameters<typeof actual.fixturePackets>) {
            for (const bytes of actual.fixturePackets(...args)) yield native.bufferPackets ? new BufferLike(bytes) : bytes
        } }
})

beforeEach(() => {
    native.invoke.mockReset()
    native.remove.mockReset()
    native.bytes.fill(0)
    native.offset = native.length = 0
    native.bufferPackets = false
    native.nonPlainWrites = 0
    native.corruptBinary = false
})
function installNative(corrupt = false) {
    const plan = planFixture(2_500_000)
    let finalized = false
    let revision = 7
    const active = new Map<string, ReturnType<typeof fixtureCharacter>>()
    const retired = new Set<string>()
    native.retireActive = () => {
        for (const character of active.values()) retired.add(character.chaId)
        active.clear()
        native.activeIds = []
        revision++
    }
    native.invoke.mockImplementation(async (command: string, args: any) => {
        if (command === 'pds_open') return { revision }
        if (command === 'native_file_job_start') { finalized = false; return { jobId: 'synthetic-job' } }
        if (command === 'native_file_job_finalize') {
            expect(args.expectedRevision).toBe(revision)
            const frame = native.bytes.subarray(0, native.length)
            const nameBytes = new DataView(frame.buffer).getUint32(0, true)
            const database = frame.subarray(8 + nameBytes)
            const decoded = database[10] === 8 ? gunzipSync(database.subarray(11)) : database.subarray(11)
            for (const character of unpack(decoded).characters as ReturnType<typeof fixtureCharacter>[]) {
                if (retired.has(character.chaId)) {
                    character.chaId += '-remapped'
                    for (const chat of character.chats) chat.id += '-remapped'
                }
                active.set(character.chaId, character)
            }
            native.activeIds = [...active.keys()]
            revision++
            finalized = true
            return true
        }
        if (command === 'native_file_job_status') return finalized
            ? { state: 'succeeded', phase: 'complete', result: { characterCount: active.size, sourceBytes: native.length } }
            : { state: 'running', phase: 'awaiting-activation' }
        if (command === 'pds_read_conversation') {
            const value = active.get(args.characterId)?.chats.find(chat => chat.id === args.conversationId)
            if (!value) return null
            if (corrupt) value.message[0].data = 'synthetic corruption'
            return { value }
        }
        if (command === 'pds_read_character') {
            const character = active.get(args.id)
            if (!character) return null
            // The store returns a bin as the object JavaScript makes of a Uint8Array.
            const value = JSON.parse(JSON.stringify(character))
            if (native.corruptBinary) value.syntheticBinary[0] ^= 1
            return { revision, value }
        }
        if (command === 'native_file_job_forget') return true
        throw new Error(`Unexpected command ${command}`)
    })
    return plan
}

describe('isolated legacy restore measurement', () => {
    it.each(['raw', 'gzip'] as const)('verifies %s warmup and measured imports after the reset permanently retires earlier parent IDs', async encoding => {
        installNative()
        const options = { megabytes: 100 as const, encoding,
            assertIsolatedHarness: async () => {}, report: async () => {} }
        expect(await runLegacyRestoreMeasurement(options)).toMatchObject({ phase: 'verified' })
        const warmupIds = [...native.activeIds]
        native.retireActive()
        native.bytes.fill(0)
        native.offset = native.length = 0
        expect(await runLegacyRestoreMeasurement(options)).toMatchObject({ phase: 'verified' })
        expect(native.activeIds.filter(id => warmupIds.includes(id))).toEqual([])
    })
    it.each(['raw', 'gzip'] as const)('writes upstream %s bytes and validates bounded native readback', async encoding => {
        const plan = installNative()
        const report = vi.fn(async () => {})
        const progress: ReadbackProgress[] = []
        const result = await runLegacyRestoreMeasurement({ megabytes: 100, encoding,
            assertIsolatedHarness: async () => {}, report,
            onReadbackProgress: event => { progress.push(event) },
        })
        const frame = native.bytes.subarray(0, native.length)
        const view = new DataView(frame.buffer)
        const nameBytes = view.getUint32(0, true)
        expect(new TextDecoder().decode(frame.subarray(4, 4 + nameBytes))).toBe('database.risudat')
        expect(view.getUint32(4 + nameBytes, true)).toBe(frame.length - 8 - nameBytes)
        const database = frame.subarray(8 + nameBytes)
        expect([...database.subarray(0, 11)]).toEqual([0, 82, 73, 83, 85, 83, 65, 86, 69, 0, encoding === 'raw' ? 7 : 8])
        const decoded = encoding === 'gzip' ? gunzipSync(database.subarray(11)) : database.subarray(11)
        expect(decoded.length).toBe(plan.decodedBytes)
        expect(unpack(decoded).characters).toHaveLength(plan.characterCount)
        expect(result).toMatchObject({ phase: 'verified', verifiedMessageCount: plan.messageCount,
            verifiedCharacterCount: plan.characterCount })
        expect(native.activeIds[0]).toBe(`${result.runId}-000000`)
        expect(native.invoke.mock.calls.filter(([command]) => command === 'pds_read_conversation')).toHaveLength(plan.characterCount)
        expect(native.invoke).toHaveBeenCalledWith('native_file_job_finalize', { jobId: 'synthetic-job', expectedRevision: 7 })
        expect(native.remove).toHaveBeenCalledTimes(2)
        expect(progress.at(-1)).toEqual({ stage: 14, index: plan.characterCount,
            readReturned: plan.characterCount, hashVerified: plan.characterCount, messageCount: plan.messageCount })
    })
    it.each([
        ['a rising resident size', [100_000_000, 90_000_000, 92_000_000, 96_000_000], {
            restoreBaselineBytes: 90_000_000, restoreIncrementBytes: 6_000_000, aboveTwiceDecoded: true,
            memoryScope: 'native-process-resident-increment-from-restore-start-excludes-separate-webcontent' }],
        ['a flat resident size under a fixture-write peak', [100_000_000, 90_000_000, 90_000_000, 90_000_000], {
            restoreBaselineBytes: 90_000_000, restoreIncrementBytes: 0, aboveTwiceDecoded: false,
            memoryScope: 'native-process-resident-increment-from-restore-start-excludes-separate-webcontent' }],
        ['a lifetime peak alone', null, {
            restoreBaselineBytes: null, restoreIncrementBytes: null, aboveTwiceDecoded: null,
            memoryScope: 'lifetime-high-water-only-cannot-separate-restore-from-fixture-write' }],
    ] as const)('gates on the resident increment from the restore start with %s', async (_name, resident, expected) => {
        installNative()
        const taken: { fixtureBytes: number; jobStarted: boolean }[] = []
        const sampleMemory = async () => {
            const residentBytes = resident?.[Math.min(taken.length, resident.length - 1)]
            taken.push({ fixtureBytes: native.length,
                jobStarted: native.invoke.mock.calls.some(([command]) => command === 'native_file_job_start') })
            return { source: 'synthetic-test', peakRssBytes: 700_000_000, ...(residentBytes === undefined ? {} : { residentBytes }) }
        }
        const result = await runLegacyRestoreMeasurement({ megabytes: 100, encoding: 'raw',
            assertIsolatedHarness: async () => {}, report: async () => {}, sampleMemory })
        expect(result).toMatchObject({ phase: 'verified', ...expected, peakRssBytes: 700_000_000,
            peakScope: 'native-process-lifetime-high-water-includes-fixture-write' })
        // Both samples before the restore see the whole fixture on disk.
        expect(taken).toEqual([
            { fixtureBytes: native.length, jobStarted: false }, { fixtureBytes: native.length, jobStarted: false },
            { fixtureBytes: native.length, jobStarted: true }, { fixtureBytes: native.length, jobStarted: true },
        ])
    })
    it('writes plain byte arrays when the encoder returns Buffer instances', async () => {
        installNative()
        native.bufferPackets = true
        expect(await runLegacyRestoreMeasurement({ megabytes: 100, encoding: 'raw',
            assertIsolatedHarness: async () => {}, report: async () => {} })).toMatchObject({ phase: 'verified' })
        expect(native.nonPlainWrites).toBe(0)
    })
    it('gates native restore after preparation and readback after the terminal interval', async () => {
        installNative()
        let prepared: Record<string, unknown> | undefined
        let terminal: Record<string, unknown> | undefined
        let releaseRestore!: () => void
        let releaseReadback!: () => void
        const restoreGate = new Promise<void>(resolve => { releaseRestore = resolve })
        const readbackGate = new Promise<void>(resolve => { releaseReadback = resolve })
        let releaseNativeRead!: () => void
        const nativeReadGate = new Promise<void>(resolve => { releaseNativeRead = resolve })
        const invoke = native.invoke.getMockImplementation()!
        let firstRead = true
        native.invoke.mockImplementation(async (command, args) => {
            if (command === 'pds_read_conversation' && firstRead) {
                firstRead = false
                await nativeReadGate
            }
            return invoke(command, args)
        })
        const progress: ReadbackProgress[] = []
        const measurement = runLegacyRestoreMeasurement({ megabytes: 100, encoding: 'raw',
            assertIsolatedHarness: async () => {}, report: async () => {},
            beforeRestore: async event => { prepared = event; await restoreGate },
            afterRestore: async event => { terminal = event; await readbackGate },
            onReadbackProgress: event => { progress.push(event) },
        })
        await vi.waitFor(() => expect(prepared?.phase).toBe('prepared'), { timeout: 10_000 })
        expect(native.length).toBeGreaterThan(2_500_000)
        expect(native.invoke.mock.calls.some(([command]) => command === 'native_file_job_start')).toBe(false)
        releaseRestore()
        await vi.waitFor(() => expect(terminal?.phase).toBe('restore-terminal'), { timeout: 10_000 })
        expect(terminal?.outcome).toBe('succeeded')
        expect(native.invoke.mock.calls.some(([command]) => command === 'pds_read_conversation')).toBe(false)
        expect(native.remove).not.toHaveBeenCalled()
        expect(progress).toEqual([])
        releaseReadback()
        await vi.waitFor(() => expect(progress.at(-1)).toMatchObject({ stage: 4, index: 0 }), { timeout: 10_000 })
        expect(progress).toEqual([{ stage: 4, index: 0, readReturned: 0, hashVerified: 0, messageCount: 0 }])
        expect(native.remove).not.toHaveBeenCalled()
        releaseNativeRead()
        expect(await measurement).toMatchObject({ phase: 'verified' })
        expect(progress.slice(0, 3).map(event => event.stage)).toEqual([4, 5, 6])
        expect(native.invoke.mock.calls.some(([command]) => command === 'pds_read_conversation')).toBe(true)
        expect(native.remove).toHaveBeenCalledTimes(2)
    })
    it('verifies one restored bin when the characters carry them', async () => {
        installNative()
        const options = { megabytes: 100 as const, encoding: 'raw' as const, binaryBytes: 4_096,
            assertIsolatedHarness: async () => {}, report: async () => {} }
        expect(await runLegacyRestoreMeasurement(options)).toMatchObject({ phase: 'verified', binaryBytes: 4_096 })
        expect(native.invoke.mock.calls.filter(([command]) => command === 'pds_read_character')).toHaveLength(1)
        native.retireActive()
        native.bytes.fill(0)
        native.offset = native.length = 0
        native.corruptBinary = true
        await expect(runLegacyRestoreMeasurement(options)).rejects.toThrow('binary mismatch')
    })
    it('rejects incorrect readback without reporting verification', async () => {
        installNative(true)
        const events: Record<string, unknown>[] = []
        const progress: ReadbackProgress[] = []
        await expect(runLegacyRestoreMeasurement({ megabytes: 100, encoding: 'raw',
            assertIsolatedHarness: async () => {}, report: async event => { events.push(event) },
            onReadbackProgress: event => { progress.push(event) },
        })).rejects.toThrow('message hash mismatch')
        expect(events.some(event => event.phase === 'verified')).toBe(false)
        expect(native.remove).not.toHaveBeenCalled()
        expect(progress.at(-1)).toEqual({ stage: 5, index: 0, readReturned: 1, hashVerified: 0, messageCount: 0 })
    })
    it('requires isolation before writing or invoking native commands', async () => {
        await expect(runLegacyRestoreMeasurement({ megabytes: 100, encoding: 'raw',
            assertIsolatedHarness: async () => { throw new Error('isolation required') }, report: async () => {},
        })).rejects.toThrow('isolation required')
        expect(native.length).toBe(0)
        expect(native.invoke).not.toHaveBeenCalled()
    })
})
