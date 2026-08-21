import type { DataRevision } from '../persistentDataStore'

export type ManifestRecord =
    | {
          revision: DataRevision
          tombstone: false
          hash: string
          size: number
          blobHashes: readonly string[]
      }
    | {
          revision: DataRevision
          tombstone: true
      }

export interface SyncManifest {
    generation: string
    records: Readonly<Record<string, ManifestRecord>>
    blobs: Readonly<Record<string, { size: number }>>
}

export interface ManifestDeltaCommit {
    expectedGeneration: string
    uploadRecordKeys: readonly string[]
    uploadBlobHashes: readonly string[]
    tombstoneKeys: readonly string[]
    nextRecords: Readonly<Record<string, ManifestRecord>>
    nextBlobs: Readonly<Record<string, { size: number }>>
}

export type ManifestDeltaPlan =
    | { kind: 'ready'; commit: ManifestDeltaCommit }
    | { kind: 'conflict'; keys: readonly string[] }

export interface ManifestDeltaTransport {
    compareAndSwap(commit: ManifestDeltaCommit): Promise<
        | { kind: 'committed'; generation: string }
        | { kind: 'stale'; actualGeneration: string }
    >
}

type LiveManifestRecord = Extract<ManifestRecord, { tombstone: false }>

function isLiveRecord(record: ManifestRecord): record is LiveManifestRecord {
    return record.tombstone === false
}

function sortedUnique(values: readonly string[]): string[] {
    return [...new Set(values)].sort()
}

function sortedValues(values: readonly string[]): string[] {
    return [...values].sort()
}

function sameRecord(
    left: ManifestRecord | undefined,
    right: ManifestRecord | undefined,
): boolean {
    if (left === undefined || right === undefined) {
        return left === right
    }
    if (!isLiveRecord(left) || !isLiveRecord(right)) {
        return !isLiveRecord(left) && !isLiveRecord(right)
    }
    if (left.hash !== right.hash || left.size !== right.size) {
        return false
    }
    const leftBlobs = sortedValues(left.blobHashes)
    const rightBlobs = sortedValues(right.blobHashes)
    return leftBlobs.length === rightBlobs.length
        && leftBlobs.every((hash, index) => hash === rightBlobs[index])
}

function cloneRecord(record: ManifestRecord): ManifestRecord {
    if (!isLiveRecord(record)) {
        return { revision: record.revision, tombstone: true }
    }
    return {
        revision: record.revision,
        tombstone: false,
        hash: record.hash,
        size: record.size,
        blobHashes: sortedValues(record.blobHashes),
    }
}

function resolveRecord(
    records: SyncManifest['records'],
    baseRecord: ManifestRecord | undefined,
    key: string,
): ManifestRecord | undefined {
    return records[key] ?? baseRecord
}

export function planManifestDelta(input: {
    base: SyncManifest
    local: SyncManifest
    remote: SyncManifest
}): ManifestDeltaPlan {
    const keys = sortedUnique([
        ...Object.keys(input.base.records),
        ...Object.keys(input.local.records),
        ...Object.keys(input.remote.records),
    ])
    const conflicts: string[] = []
    const selectedRecords = new Map<string, ManifestRecord>()
    const locallyUploadedRecords = new Map<string, ManifestRecord>()
    const tombstoneKeys: string[] = []

    for (const key of keys) {
        const baseRecord = input.base.records[key]
        const localRecord = resolveRecord(input.local.records, baseRecord, key)
        const remoteRecord = resolveRecord(input.remote.records, baseRecord, key)
        const localChanged = !sameRecord(localRecord, baseRecord)
        const remoteChanged = !sameRecord(remoteRecord, baseRecord)

        if (localChanged && remoteChanged && !sameRecord(localRecord, remoteRecord)) {
            conflicts.push(key)
            continue
        }

        const selected = localChanged && !remoteChanged
            ? localRecord
            : remoteRecord ?? localRecord ?? baseRecord
        if (selected) {
            selectedRecords.set(key, cloneRecord(selected))
        }
        if (localChanged && !remoteChanged && localRecord) {
            if (localRecord.tombstone) {
                tombstoneKeys.push(key)
            } else {
                locallyUploadedRecords.set(key, localRecord)
            }
        }
    }

    if (conflicts.length > 0) {
        return { kind: 'conflict', keys: conflicts }
    }

    const uploadBlobHashes = sortedUnique(
        [...locallyUploadedRecords.values()]
            .flatMap((record) => isLiveRecord(record) ? record.blobHashes : [])
            .filter((hash) => input.remote.blobs[hash] === undefined),
    )
    const referencedBlobHashes = sortedUnique(
        [...selectedRecords.values()]
            .flatMap((record) => isLiveRecord(record) ? record.blobHashes : []),
    )
    const nextBlobs = new Map<string, { size: number }>()
    for (const hash of Object.keys(input.remote.blobs).sort()) {
        nextBlobs.set(hash, { size: input.remote.blobs[hash].size })
    }
    for (const hash of referencedBlobHashes) {
        const metadata = input.remote.blobs[hash] ?? input.local.blobs[hash]
        if (!metadata) {
            throw new Error(`Manifest delta input is missing metadata for blob: ${hash}`)
        }
        nextBlobs.set(hash, { size: metadata.size })
    }

    return {
        kind: 'ready',
        commit: {
            expectedGeneration: input.remote.generation,
            uploadRecordKeys: [...locallyUploadedRecords.keys()].sort(),
            uploadBlobHashes,
            tombstoneKeys,
            nextRecords: Object.fromEntries(selectedRecords),
            nextBlobs: Object.fromEntries([...nextBlobs].sort(([left], [right]) =>
                left < right ? -1 : left > right ? 1 : 0,
            )),
        },
    }
}

export function publishManifestDelta(
    plan: Extract<ManifestDeltaPlan, { kind: 'ready' }>,
    transport: ManifestDeltaTransport,
): Promise<
    | { kind: 'committed'; generation: string }
    | { kind: 'stale'; actualGeneration: string }
> {
    return transport.compareAndSwap(plan.commit)
}
