import {
    hashLogicalManifest,
    validateLogicalManifest,
    type LogicalManifest,
    type LogicalManifestObject,
    type LogicalManifestRecord,
} from './logicalManifest'
import type {
    LogicalDeltaApplyOperation,
    LogicalDeltaConflictType,
} from './logicalDeltaPull'

const SHA256_PATTERN = /^[0-9a-f]{64}$/
const MAX_GENERATION_BYTES = 1024
const textEncoder = new TextEncoder()

export type BidirectionalRecordOperation = LogicalDeltaApplyOperation

export type BidirectionalSyncConflictType = 'same-record' | 'delete-vs-edit'

export interface BidirectionalSyncConflictPlan {
    kind: 'conflict'
    libraryId: string
    expectedLocalRevision: number
    expectedRemoteGeneration: string
    baseManifestHash: string
    localManifestHash: string
    remoteManifestHash: string
    conflicts: Array<{ key: string; type: BidirectionalSyncConflictType }>
    replacementAllowed: false
    requiredBackup: 'complete-lossless-package'
    contentBytes: 0
}

export interface BidirectionalSyncReadyPlan {
    kind: 'ready'
    libraryId: string
    expectedLocalRevision: number
    expectedRemoteGeneration: string
    baseManifestHash: string
    localManifestHash: string
    remoteManifestHash: string
    localApply: BidirectionalRecordOperation[]
    remoteApply: BidirectionalRecordOperation[]
    uploadObjects: LogicalManifestObject[]
    downloadObjects: LogicalManifestObject[]
    contentBytes: {
        upload: number
        download: number
        total: number
    }
    isNoOp: boolean
}

export interface BidirectionalSyncStalePlan {
    kind: 'stale'
    expectedLocalRevision: number
    actualLocalRevision: number
    expectedRemoteGeneration: string
    actualRemoteGeneration: string
    failures: Array<'remote-generation' | 'local-revision'>
    contentBytes: 0
}

export type BidirectionalSyncPlan =
    | BidirectionalSyncReadyPlan
    | BidirectionalSyncConflictPlan
    | BidirectionalSyncStalePlan

interface RecordTriple {
    key: string
    base?: LogicalManifestRecord
    local?: LogicalManifestRecord
    remote?: LogicalManifestRecord
}

function recordsEqual(
    left: LogicalManifestRecord | undefined,
    right: LogicalManifestRecord | undefined,
): boolean {
    if (left === undefined || right === undefined) return left === right
    if (left.state === 'tombstone' || right.state === 'tombstone') {
        return left.state === 'tombstone'
            && right.state === 'tombstone'
            && left.deletedGenerationSequence === right.deletedGenerationSequence
    }
    return left.objectHash === right.objectHash
        && left.dependencies.length === right.dependencies.length
        && left.dependencies.every((hash, index) => hash === right.dependencies[index])
}

function* recordUnion(
    base: readonly LogicalManifestRecord[],
    local: readonly LogicalManifestRecord[],
    remote: readonly LogicalManifestRecord[],
): Generator<RecordTriple> {
    let baseIndex = 0
    let localIndex = 0
    let remoteIndex = 0
    while (baseIndex < base.length || localIndex < local.length || remoteIndex < remote.length) {
        const keys = [
            base[baseIndex]?.key,
            local[localIndex]?.key,
            remote[remoteIndex]?.key,
        ].filter((key): key is string => key !== undefined)
        let key = keys[0]
        for (const candidate of keys.slice(1)) {
            if (candidate < key) key = candidate
        }
        yield {
            key,
            base: base[baseIndex]?.key === key ? base[baseIndex++] : undefined,
            local: local[localIndex]?.key === key ? local[localIndex++] : undefined,
            remote: remote[remoteIndex]?.key === key ? remote[remoteIndex++] : undefined,
        }
    }
}

function compareSequences(left: string, right: string): number {
    if (left.length !== right.length) return left.length < right.length ? -1 : 1
    return left < right ? -1 : left > right ? 1 : 0
}

function validateDescendant(
    base: LogicalManifest,
    descendant: LogicalManifest,
    descendantHash: string,
    baseHash: string,
    description: string,
): void {
    if (compareSequences(descendant.generationSequence, base.generationSequence) < 0) {
        throw new TypeError(`Bidirectional sync ${description} generation predates the common base`)
    }
    if (
        descendant.generationSequence === base.generationSequence
        && (descendant.generation !== base.generation || descendantHash !== baseHash)
    ) {
        throw new TypeError(`Bidirectional sync ${description} reuses the common-base sequence`)
    }
}

function validateObjectSizeParity(manifests: readonly LogicalManifest[]): void {
    const sizes = new Map<string, number>()
    for (const manifest of manifests) {
        for (const object of manifest.objects) {
            const previous = sizes.get(object.hash)
            if (previous !== undefined && previous !== object.size) {
                throw new TypeError(`Logical manifests disagree on object size for ${object.hash}`)
            }
            sizes.set(object.hash, object.size)
        }
    }
}

function operationFor(record: LogicalManifestRecord): BidirectionalRecordOperation {
    if (record.state === 'tombstone') {
        return {
            type: 'delete',
            key: record.key,
            deletedGenerationSequence: record.deletedGenerationSequence,
        }
    }
    return {
        type: 'put',
        key: record.key,
        objectHash: record.objectHash,
        dependencies: [...record.dependencies],
    }
}

function conflictTypeFor(
    local: LogicalManifestRecord | undefined,
    remote: LogicalManifestRecord | undefined,
): BidirectionalSyncConflictType {
    if (local?.state === 'tombstone' && remote?.state === 'tombstone') {
        return 'same-record'
    }
    const p4ConflictType: LogicalDeltaConflictType = local?.state === 'live'
        && remote?.state === 'live'
        ? 'live-live'
        : 'delete-edit'
    return p4ConflictType === 'live-live' ? 'same-record' : 'delete-vs-edit'
}

function objectMap(manifest: LogicalManifest): Map<string, LogicalManifestObject> {
    return new Map(manifest.objects.map((object) => [object.hash, object]))
}

function addMissingObjects(
    output: Map<string, LogicalManifestObject>,
    record: LogicalManifestRecord,
    sourceObjects: ReadonlyMap<string, LogicalManifestObject>,
    targetObjects: ReadonlyMap<string, LogicalManifestObject>,
): void {
    if (record.state === 'tombstone') return
    for (const hash of [record.objectHash, ...record.dependencies]) {
        if (targetObjects.has(hash)) continue
        const object = sourceObjects.get(hash)
        if (!object) throw new TypeError(`Logical manifest is missing referenced object ${hash}`)
        output.set(hash, { ...object })
    }
}

function sumObjectSizes(objects: readonly LogicalManifestObject[], description: string): number {
    let total = 0
    for (const object of objects) {
        total += object.size
        if (!Number.isSafeInteger(total)) {
            throw new TypeError(`Bidirectional sync ${description} bytes exceed the safe integer limit`)
        }
    }
    return total
}

function validateExpectedGeneration(value: unknown): string {
    if (
        typeof value !== 'string'
        || value.length === 0
        || textEncoder.encode(value).byteLength > MAX_GENERATION_BYTES
    ) {
        throw new TypeError('Bidirectional sync expected remote generation is invalid')
    }
    return value
}

export async function planBidirectionalSync(input: {
    baseManifestHash: string
    base: LogicalManifest
    local: LogicalManifest
    remote: LogicalManifest
    expectedLocalRevision: number
    expectedRemoteGeneration: string
}): Promise<BidirectionalSyncPlan> {
    if (!SHA256_PATTERN.test(input.baseManifestHash)) {
        throw new TypeError('Bidirectional sync common base hash must be a lowercase SHA-256')
    }
    if (!Number.isSafeInteger(input.expectedLocalRevision) || input.expectedLocalRevision < 0) {
        throw new TypeError('Bidirectional sync expected local revision is invalid')
    }
    const expectedRemoteGeneration = validateExpectedGeneration(input.expectedRemoteGeneration)
    const base = validateLogicalManifest(input.base)
    const local = validateLogicalManifest(input.local)
    const remote = validateLogicalManifest(input.remote)

    const failures: BidirectionalSyncStalePlan['failures'] = []
    if (remote.generation !== expectedRemoteGeneration) failures.push('remote-generation')
    if (local.sourceRevision !== input.expectedLocalRevision) failures.push('local-revision')
    if (failures.length > 0) {
        return {
            kind: 'stale',
            expectedLocalRevision: input.expectedLocalRevision,
            actualLocalRevision: local.sourceRevision,
            expectedRemoteGeneration,
            actualRemoteGeneration: remote.generation,
            failures,
            contentBytes: 0,
        }
    }

    if (base.libraryId !== local.libraryId || base.libraryId !== remote.libraryId) {
        throw new TypeError('Bidirectional sync manifests belong to different libraries')
    }
    const baseManifestHash = await hashLogicalManifest(base)
    if (baseManifestHash !== input.baseManifestHash) {
        throw new TypeError('Bidirectional sync common base hash does not match the supplied base')
    }
    const [localManifestHash, remoteManifestHash] = await Promise.all([
        hashLogicalManifest(local),
        hashLogicalManifest(remote),
    ])
    validateDescendant(base, local, localManifestHash, baseManifestHash, 'local')
    validateDescendant(base, remote, remoteManifestHash, baseManifestHash, 'remote')
    validateObjectSizeParity([base, local, remote])

    const localApply: BidirectionalRecordOperation[] = []
    const remoteApply: BidirectionalRecordOperation[] = []
    const conflicts: BidirectionalSyncConflictPlan['conflicts'] = []
    const uploadObjects = new Map<string, LogicalManifestObject>()
    const downloadObjects = new Map<string, LogicalManifestObject>()
    const localObjects = objectMap(local)
    const remoteObjects = objectMap(remote)

    for (const records of recordUnion(base.records, local.records, remote.records)) {
        if (
            records.base?.state === 'live'
            && (records.local === undefined || records.remote === undefined)
        ) {
            throw new TypeError(
                `Bidirectional sync descendant must tombstone deleted base record ${records.key}`,
            )
        }
        const localChanged = !recordsEqual(records.local, records.base)
        const remoteChanged = !recordsEqual(records.remote, records.base)
        if (!localChanged && !remoteChanged) continue
        if (localChanged && remoteChanged) {
            if (recordsEqual(records.local, records.remote)) continue
            conflicts.push({
                key: records.key,
                type: conflictTypeFor(records.local, records.remote),
            })
            continue
        }
        if (localChanged && records.local) {
            remoteApply.push(operationFor(records.local))
            addMissingObjects(uploadObjects, records.local, localObjects, remoteObjects)
        } else if (remoteChanged && records.remote) {
            localApply.push(operationFor(records.remote))
            addMissingObjects(downloadObjects, records.remote, remoteObjects, localObjects)
        }
    }

    if (conflicts.length > 0) {
        return {
            kind: 'conflict',
            libraryId: base.libraryId,
            expectedLocalRevision: input.expectedLocalRevision,
            expectedRemoteGeneration,
            baseManifestHash,
            localManifestHash,
            remoteManifestHash,
            conflicts,
            replacementAllowed: false,
            requiredBackup: 'complete-lossless-package',
            contentBytes: 0,
        }
    }

    const uploads = [...uploadObjects.values()].sort((left, right) =>
        left.hash < right.hash ? -1 : left.hash > right.hash ? 1 : 0,
    )
    const downloads = [...downloadObjects.values()].sort((left, right) =>
        left.hash < right.hash ? -1 : left.hash > right.hash ? 1 : 0,
    )
    const uploadBytes = sumObjectSizes(uploads, 'upload')
    const downloadBytes = sumObjectSizes(downloads, 'download')
    const totalBytes = uploadBytes + downloadBytes
    if (!Number.isSafeInteger(totalBytes)) {
        throw new TypeError('Bidirectional sync total content bytes exceed the safe integer limit')
    }
    return {
        kind: 'ready',
        libraryId: base.libraryId,
        expectedLocalRevision: input.expectedLocalRevision,
        expectedRemoteGeneration,
        baseManifestHash,
        localManifestHash,
        remoteManifestHash,
        localApply,
        remoteApply,
        uploadObjects: uploads,
        downloadObjects: downloads,
        contentBytes: {
            upload: uploadBytes,
            download: downloadBytes,
            total: totalBytes,
        },
        isNoOp: localApply.length === 0
            && remoteApply.length === 0
            && uploads.length === 0
            && downloads.length === 0,
    }
}
