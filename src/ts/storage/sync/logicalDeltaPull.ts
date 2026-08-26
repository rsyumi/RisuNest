import {
    hashLogicalManifest,
    validateLogicalManifest,
    type LogicalManifest,
    type LogicalManifestRecord,
} from './logicalManifest'

const SHA256_PATTERN = /^[0-9a-f]{64}$/

export type LogicalDeltaApplyOperation =
    | {
          type: 'put'
          key: string
          objectHash: string
          dependencies: string[]
      }
    | {
          type: 'delete'
          key: string
          deletedGenerationSequence: string
      }

export type LogicalDeltaConflictType = 'live-live' | 'delete-edit'

export type LogicalDeltaPullPlan =
    | {
          kind: 'ready'
          expectedLocalRevision: number
          expectedBaseManifestHash: string
          expectedRemoteGeneration: string
          nextBaseGenerationSequence: string
          apply: LogicalDeltaApplyOperation[]
          preserveLocalKeys: string[]
          candidateObjectHashes: string[]
          nextBaseManifestHash: string
      }
    | {
          kind: 'conflict'
          expectedLocalRevision: number
          expectedBaseManifestHash: string
          conflicts: Array<{ key: string; type: LogicalDeltaConflictType }>
      }

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
    while (
        baseIndex < base.length
        || localIndex < local.length
        || remoteIndex < remote.length
    ) {
        const candidateKeys = [
            base[baseIndex]?.key,
            local[localIndex]?.key,
            remote[remoteIndex]?.key,
        ].filter((key): key is string => key !== undefined)
        let key = candidateKeys[0]
        for (const candidate of candidateKeys.slice(1)) {
            if (candidate < key) key = candidate
        }
        const baseRecord = base[baseIndex]?.key === key ? base[baseIndex++] : undefined
        const localRecord = local[localIndex]?.key === key ? local[localIndex++] : undefined
        const remoteRecord = remote[remoteIndex]?.key === key ? remote[remoteIndex++] : undefined
        yield { key, base: baseRecord, local: localRecord, remote: remoteRecord }
    }
}

function compareSequences(left: string, right: string): number {
    if (left.length !== right.length) return left.length < right.length ? -1 : 1
    return left < right ? -1 : left > right ? 1 : 0
}

function validateObjectSizeParity(manifests: readonly LogicalManifest[]): void {
    const positions = manifests.map(() => 0)
    while (positions.some((position, index) => position < manifests[index].objects.length)) {
        let hash: string | undefined
        for (let index = 0; index < manifests.length; index++) {
            const candidate = manifests[index].objects[positions[index]]?.hash
            if (candidate !== undefined && (hash === undefined || candidate < hash)) hash = candidate
        }
        let size: number | undefined
        for (let index = 0; index < manifests.length; index++) {
            const object = manifests[index].objects[positions[index]]
            if (object?.hash !== hash) continue
            if (size !== undefined && size !== object.size) {
                throw new TypeError(`Logical manifests disagree on object size for ${hash}`)
            }
            size = object.size
            positions[index] += 1
        }
    }
}

function addCandidateHashes(
    candidates: Set<string>,
    record: Extract<LogicalManifestRecord, { state: 'live' }>,
): void {
    candidates.add(record.objectHash)
    for (const dependency of record.dependencies) candidates.add(dependency)
}

export async function planLogicalDeltaPull(input: {
    baseManifestHash: string
    base: LogicalManifest
    local: LogicalManifest
    remote: LogicalManifest
    expectedLocalRevision: number
}): Promise<LogicalDeltaPullPlan> {
    if (!SHA256_PATTERN.test(input.baseManifestHash)) {
        throw new TypeError('Logical delta base manifest hash must be a lowercase SHA-256')
    }
    if (!Number.isSafeInteger(input.expectedLocalRevision) || input.expectedLocalRevision < 0) {
        throw new TypeError('Logical delta expected local revision is invalid')
    }

    const base = validateLogicalManifest(input.base)
    const local = validateLogicalManifest(input.local)
    const remote = validateLogicalManifest(input.remote)
    if (local.sourceRevision !== input.expectedLocalRevision) {
        throw new TypeError('Logical delta expected local revision does not match the local manifest')
    }
    if (base.libraryId !== local.libraryId || base.libraryId !== remote.libraryId) {
        throw new TypeError('Logical delta manifests belong to different libraries')
    }
    if (compareSequences(remote.generationSequence, base.generationSequence) < 0) {
        throw new TypeError('Logical delta remote generation predates the common base')
    }

    const actualBaseManifestHash = await hashLogicalManifest(base)
    if (actualBaseManifestHash !== input.baseManifestHash) {
        throw new TypeError('Logical delta base manifest hash does not match the common base')
    }
    const localManifestHash = await hashLogicalManifest(local)
    const remoteManifestHash = await hashLogicalManifest(remote)
    if (
        (
            local.generation === base.generation
            && (
                localManifestHash !== actualBaseManifestHash
                || local.generationSequence !== base.generationSequence
            )
        )
        || (
            remote.generation === base.generation
            && (
                remoteManifestHash !== actualBaseManifestHash
                || remote.generationSequence !== base.generationSequence
            )
        )
        || (
            remote.generation === local.generation
            && (
                remoteManifestHash !== localManifestHash
                || remote.generationSequence !== local.generationSequence
            )
        )
    ) {
        throw new TypeError('Logical delta generation ID reuses different content')
    }
    if (
        remote.generationSequence === base.generationSequence
        && (
            remote.generation !== base.generation
            || remoteManifestHash !== actualBaseManifestHash
        )
    ) {
        throw new TypeError('Logical delta remote generation reuses the common-base sequence')
    }
    validateObjectSizeParity([base, local, remote])

    const apply: LogicalDeltaApplyOperation[] = []
    const preserveLocalKeys: string[] = []
    const conflicts: Array<{ key: string; type: LogicalDeltaConflictType }> = []
    const candidateObjectHashes = new Set<string>()

    for (const records of recordUnion(base.records, local.records, remote.records)) {
        if (records.base?.state === 'tombstone') {
            if (records.local?.state !== 'tombstone' || records.remote?.state !== 'tombstone') {
                throw new TypeError(
                    `Logical delta descendant must retain tombstone ${records.key}`,
                )
            }
        } else if (records.base?.state === 'live') {
            if (records.local === undefined || records.remote === undefined) {
                throw new TypeError(
                    `Logical delta descendant must tombstone deleted base record ${records.key}`,
                )
            }
        }
        const localChanged = !recordsEqual(records.local, records.base)
        const remoteChanged = !recordsEqual(records.remote, records.base)
        if (!localChanged && !remoteChanged) continue

        if (localChanged && remoteChanged) {
            if (recordsEqual(records.local, records.remote)) continue
            conflicts.push({
                key: records.key,
                type: records.local?.state === 'live' && records.remote?.state === 'live'
                    ? 'live-live'
                    : 'delete-edit',
            })
            continue
        }
        if (localChanged) {
            preserveLocalKeys.push(records.key)
            continue
        }
        if (records.remote?.state === 'live') {
            apply.push({
                type: 'put',
                key: records.key,
                objectHash: records.remote.objectHash,
                dependencies: [...records.remote.dependencies],
            })
            addCandidateHashes(candidateObjectHashes, records.remote)
        } else if (records.remote?.state === 'tombstone') {
            apply.push({
                type: 'delete',
                key: records.key,
                deletedGenerationSequence: records.remote.deletedGenerationSequence,
            })
        }
    }

    if (conflicts.length > 0) {
        return {
            kind: 'conflict',
            expectedLocalRevision: input.expectedLocalRevision,
            expectedBaseManifestHash: input.baseManifestHash,
            conflicts,
        }
    }
    return {
        kind: 'ready',
        expectedLocalRevision: input.expectedLocalRevision,
        expectedBaseManifestHash: input.baseManifestHash,
        expectedRemoteGeneration: remote.generation,
        nextBaseGenerationSequence: remote.generationSequence,
        apply,
        preserveLocalKeys,
        candidateObjectHashes: [...candidateObjectHashes].sort(),
        nextBaseManifestHash: remoteManifestHash,
    }
}
