import { invoke } from '@tauri-apps/api/core'
import { join } from '@tauri-apps/api/path'
import { open, mkdir, remove, SeekMode, type FileHandle } from '@tauri-apps/plugin-fs'
import { Gzip } from 'fflate'
import { nativeDataPath } from '../../src/ts/storage/nativePaths'
import type { NativeFileJobStatus } from '../../src/ts/storage/nativeFileJobs'
import { fixturePackets, fixtureCharacter, messageProjection, planFixture, MEASUREMENT_MB,
    type LegacyEncoding, type LegacyMessage } from './fixture'

const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))
const terminal = new Set(['succeeded', 'failed', 'cancelled'])
const hash = async (text: string) => Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(text))),
    byte => byte.toString(16).padStart(2, '0')).join('')
async function write(file: FileHandle, source: Uint8Array) {
    // The WebView's Buffer polyfill serializes through toJSON as an object, which the IPC rejects.
    const bytes = Object.getPrototypeOf(source) === Uint8Array.prototype ? source : new Uint8Array(source)
    let offset = 0
    while (offset < bytes.length) {
        const count = await file.write(bytes.subarray(offset))
        if (count <= 0) throw new Error('Synthetic fixture write made no progress')
        offset += count
    }
}
export interface MeasurementOptions {
    megabytes: 100 | 300 | 600
    encoding: LegacyEncoding
    charactersFirst?: boolean
    // Bytes of the MessagePack bin each synthetic character carries.
    binaryBytes?: number
    assertIsolatedHarness(): Promise<void>
    report(event: Record<string, unknown>): Promise<void>
    sampleMemory?(): Promise<MemorySample>
    beforeRestore?(prepared: Record<string, unknown>): Promise<void>
    afterRestore?(terminal: Record<string, unknown>): Promise<void>
    onReadbackProgress?(progress: ReadbackProgress): void
}
export interface MemorySample {
    source: string
    // Resident bytes at the moment of sampling. Only this measures what the restore adds.
    residentBytes?: number
    // The process's lifetime high-water, which already includes the fixture write.
    peakRssBytes?: number
}
const RESIDENT_INCREMENT_SCOPE = 'native-process-resident-increment-from-restore-start-excludes-separate-webcontent'
const LIFETIME_PEAK_SCOPE = 'native-process-lifetime-high-water-includes-fixture-write'
export interface ReadbackProgress {
    stage: 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14
    index: number
    readReturned: number
    hashVerified: number
    messageCount: number
}
export async function runLegacyRestoreMeasurement(options: MeasurementOptions) {
    await options.assertIsolatedHarness()
    if (!MEASUREMENT_MB.includes(options.megabytes) || !['raw', 'gzip'].includes(options.encoding))
        throw new Error('Unknown synthetic legacy restore case')
    const runId = crypto.randomUUID()
    const plan = planFixture(options.megabytes * 1_000_000, runId, options.binaryBytes)
    const identity = { schema: 'risunest.synthetic-legacy-restore/v1', synthetic: true, runId,
        decodedBytes: plan.decodedBytes, encoding: options.encoding, charactersFirst: options.charactersFirst ?? true,
        characterCount: plan.characterCount, messageCount: plan.messageCount, binaryBytes: plan.binaryBytes }
    const root = await join(await nativeDataPath(), 'synthetic-legacy-restore', runId)
    await mkdir(root, { recursive: true })
    const path = await join(root, 'synthetic.bin')
    const file = await open(path, { createNew: true, write: true })
    const name = new TextEncoder().encode('database.risudat')
    const frame = new Uint8Array(8 + name.length)
    new DataView(frame.buffer).setUint32(0, name.length, true)
    frame.set(name, 4)
    let databaseBytes = 11
    let decoded = 0
    const hashes: string[] = []
    try {
        await write(file, frame)
        await write(file, Uint8Array.of(0, 82, 73, 83, 85, 83, 65, 86, 69, 0, options.encoding === 'raw' ? 7 : 8))
        const chunks: Uint8Array[] = []
        const gzip = options.encoding === 'gzip' ? new Gzip({ level: 6 }, chunk => chunks.push(chunk)) : undefined
        for (const bytes of fixturePackets(plan, options.charactersFirst ?? true)) {
            decoded += bytes.length
            if (gzip) gzip.push(bytes)
            else chunks.push(bytes)
            for (const chunk of chunks) { await write(file, chunk); databaseBytes += chunk.length }
            chunks.length = 0
        }
        gzip?.push(new Uint8Array(), true)
        for (const chunk of chunks) { await write(file, chunk); databaseBytes += chunk.length }
        if (decoded !== plan.decodedBytes) throw new Error('Decoded byte accounting mismatch')
        const length = new Uint8Array(4)
        new DataView(length.buffer).setUint32(0, databaseBytes, true)
        await file.seek(4 + name.length, SeekMode.Start)
        await write(file, length)
    } finally { await file.close() }
    for (let index = 0; index < plan.characterCount; index++) {
        hashes.push(await hash(messageProjection(fixtureCharacter(plan, index).chats[0].message)))
    }
    const sourceBytes = frame.length + databaseBytes
    const opened = await invoke<{ revision: number }>('pds_open')
    const memoryBefore = await options.sampleMemory?.()
    const prepared = { ...identity, phase: 'prepared', sourceBytes, memoryBefore }
    await options.report(prepared)
    await options.beforeRestore?.(prepared)
    // The baseline is taken with the fixture already on disk, so the gate sees only what the restore adds.
    const memoryAtStart = await options.sampleMemory?.()
    const restoreBaselineBytes = memoryAtStart?.residentBytes ?? null
    let restoreResidentMaxBytes = restoreBaselineBytes
    let peakRssBytes = memoryAtStart?.peakRssBytes ?? null
    const startedAt = Date.now()
    await options.report({ ...identity, phase: 'restore-started', startedAt, sourceBytes, memoryBefore, memoryAtStart })
    const job = await invoke<{ jobId: string }>('native_file_job_start', { request: {
        kind: 'restore-legacy-local-backup', source: { type: 'desktopPath', path }, expectedRevision: opened.revision,
    } })
    let status: NativeFileJobStatus
    let finalized = false
    let samples = 0
    let residentSamples = 0
    do {
        status = await invoke<NativeFileJobStatus>('native_file_job_status', { jobId: job.jobId })
        const memory = await options.sampleMemory?.()
        if (memory) {
            samples++
            if (memory.residentBytes !== undefined) {
                restoreResidentMaxBytes = Math.max(restoreResidentMaxBytes ?? 0, memory.residentBytes)
                residentSamples++
            }
            if (memory.peakRssBytes !== undefined) peakRssBytes = Math.max(peakRssBytes ?? 0, memory.peakRssBytes)
        }
        if (status.phase === 'awaiting-activation' && !finalized) {
            await invoke('native_file_job_finalize', { jobId: job.jobId, expectedRevision: opened.revision })
            finalized = true
        }
        if (!terminal.has(status.state)) await pause(200)
    } while (!terminal.has(status.state))
    const finishedAt = Date.now()
    const restoreIncrementBytes = restoreBaselineBytes === null || residentSamples === 0 ? null
        : Math.max(0, (restoreResidentMaxBytes ?? 0) - restoreBaselineBytes)
    const measurement = { ...identity, phase: 'restore-terminal', startedAt, finishedAt,
        elapsedMs: finishedAt - startedAt, sourceBytes, outcome: status.state,
        code: status.error?.code ?? null, memorySamples: samples,
        memorySource: memoryBefore?.source ?? 'external-sampler-required',
        memoryScope: restoreIncrementBytes !== null ? RESIDENT_INCREMENT_SCOPE
            : peakRssBytes !== null ? 'lifetime-high-water-only-cannot-separate-restore-from-fixture-write' : 'not-sampled',
        restoreBaselineBytes, restoreResidentMaxBytes, restoreIncrementBytes,
        incrementToDecodedRatio: restoreIncrementBytes === null ? null : restoreIncrementBytes / plan.decodedBytes,
        aboveTwiceDecoded: restoreIncrementBytes === null ? null : restoreIncrementBytes > 2 * plan.decodedBytes,
        peakRssBytes, peakScope: LIFETIME_PEAK_SCOPE,
        peakToDecodedRatio: peakRssBytes === null ? null : peakRssBytes / plan.decodedBytes,
        peakToSourceRatio: peakRssBytes === null ? null : peakRssBytes / sourceBytes }
    await options.report(measurement)
    await options.afterRestore?.(measurement)
    if (status.state !== 'succeeded') return measurement
    if (status.result?.characterCount !== plan.characterCount || status.result.sourceBytes !== sourceBytes)
        throw new Error('Restore result counts differ from the generated fixture')
    // Read one bounded conversation at a time, after the measurement interval ends.
    let messages = 0
    let readReturned = 0
    let hashVerified = 0
    const progress = (stage: ReadbackProgress['stage'], index: number) =>
        options.onReadbackProgress?.({ stage, index, readReturned, hashVerified, messageCount: messages })
    for (let index = 0; index < plan.characterCount; index++) {
        const expected = fixtureCharacter(plan, index)
        progress(4, index)
        const read = await invoke<{ value: { message: LegacyMessage[] } }>('pds_read_conversation', {
            characterId: expected.chaId, conversationId: expected.chats[0].id,
        })
        readReturned++
        progress(5, index)
        if (!read || await hash(messageProjection(read.value.message)) !== hashes[index])
            throw new Error('Restored synthetic message hash mismatch')
        messages += read.value.message.length
        hashVerified++
        progress(6, index)
    }
    if (messages !== plan.messageCount) throw new Error('Restored synthetic message count mismatch')
    if (plan.binaryBytes > 0) {
        // A bin is stored as the object JavaScript makes of a Uint8Array, one member per byte.
        const expected = fixtureCharacter(plan, plan.fullCharacterCount)
        const read = await invoke<{ value: Record<string, unknown> } | null>('pds_read_character', { id: expected.chaId })
        if (JSON.stringify(read?.value.syntheticBinary) !== JSON.stringify(expected.syntheticBinary))
            throw new Error('Restored synthetic binary mismatch')
    }
    progress(7, plan.characterCount)
    await invoke('native_file_job_forget', { jobId: job.jobId })
    progress(8, plan.characterCount)
    const complete = { ...measurement, phase: 'verified', verifiedMessageCount: messages, verifiedCharacterCount: hashes.length }
    progress(9, plan.characterCount)
    await options.report(complete)
    progress(10, plan.characterCount)
    progress(11, plan.characterCount)
    await remove(path)
    progress(12, plan.characterCount)
    progress(13, plan.characterCount)
    await remove(root)
    progress(14, plan.characterCount)
    return complete
}
