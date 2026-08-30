import { bench, describe } from 'vitest'

import type { BlobWriteMetadata } from './blobStore'
import type {
    RuntimeAssetRepositoryDispatcher,
    StagedRuntimeAssetWrite,
} from './assetRepositoryRuntime'
import { createCoordinatorOwnedAssetBlobStore } from './assetRepositoryRuntime'
import type { PersistentStorageAuthority } from './persistentStorageAuthority'

const metadata: BlobWriteMetadata = {
    kind: 'asset',
    mime: 'application/octet-stream',
    name: 'benchmark.bin',
    ext: 'bin',
}

interface CoordinationMeasurement {
    bytes: number
    totalMs: number
    queueOccupancyMs: number
    coordinatorProbeMs: number
}

function createHarness() {
    let revision = 1
    let coordinatorTail = Promise.resolve()
    let resolvePreparationStarted!: () => void
    const preparationStarted = new Promise<void>((resolve) => {
        resolvePreparationStarted = resolve
    })
    const queueOccupancies: number[] = []
    const runtime = {
        getStorageAuthorityEpoch: () => 1,
        runStorageOnlyMutation(operation: (expectedRevision: number) => Promise<number>) {
            const run = coordinatorTail.then(async () => {
                const startedAt = performance.now()
                revision = await operation(revision)
                queueOccupancies.push(performance.now() - startedAt)
            })
            coordinatorTail = run.catch(() => undefined)
            return run
        },
    }
    const dispatcher = {
        async stagePut(key, ownedData, stagedMetadata) {
            resolvePreparationStarted()
            await crypto.subtle.digest('SHA-256', ownedData.slice().buffer as ArrayBuffer)
            return { authority: { format: 'legacy' }, key, ownedData, stagedMetadata }
        },
        async stageNewInlayImage() {
            throw new Error('benchmark does not stage Inlay images')
        },
        async activateStagedWrite(staged: StagedRuntimeAssetWrite & {
            key: string
            ownedData: Uint8Array
            stagedMetadata: BlobWriteMetadata
        }) {
            revision++
            return {
                ...staged.stagedMetadata,
                key: staged.key,
                size: staged.ownedData.byteLength,
            }
        },
        async abortStagedWrite() {},
        async put() { throw new Error('benchmark requires staged put') },
        async putNewInlayImage() { throw new Error('benchmark requires staged put') },
        async read() { return null },
        async stat() { return null },
        async list() { return [] },
        async remove() {},
        async resolveUrl() { return null },
    } as RuntimeAssetRepositoryDispatcher
    const authority = {
        rawStore: {
            async readRoot() { return { revision, value: {} } },
        },
        gate: {
            async runKeyedWrite(_key: string, operation: () => Promise<unknown>) {
                return operation()
            },
        },
    } as unknown as PersistentStorageAuthority
    const store = createCoordinatorOwnedAssetBlobStore(dispatcher, authority, runtime)
    return {
        preparationStarted,
        queueOccupancy: () => queueOccupancies.at(-1) ?? 0,
        runtime,
        store,
    }
}

async function measure(bytes: number): Promise<CoordinationMeasurement> {
    const harness = createHarness()
    const payload = new Uint8Array(bytes)
    payload.fill(0x5a)
    const startedAt = performance.now()
    const write = harness.store.put('assets/benchmark.bin', payload, metadata)
    await harness.preparationStarted
    const probeStartedAt = performance.now()
    await harness.runtime.runStorageOnlyMutation(async (revision) => revision)
    const coordinatorProbeMs = performance.now() - probeStartedAt
    await write
    return {
        bytes,
        totalMs: performance.now() - startedAt,
        queueOccupancyMs: harness.queueOccupancy(),
        coordinatorProbeMs,
    }
}

for (const bytes of [256 * 1024, 8 * 1024 * 1024]) {
    const measurement = await measure(bytes)
    console.log(`asset-write-coordination ${JSON.stringify(measurement)}`)

    describe(`${bytes / 1024} KiB staged native asset write`, () => {
        bench('total write with preparation outside coordinator', async () => {
            await measure(bytes)
        })
    })
}
