import { decodeLogicalRecordKey } from './logicalRecordKey'
import {
    validateGenerationSequence,
    type LogicalManifestRecord,
} from './logicalManifest'

const MAX_DEVICE_ID_CHARACTERS = 1024
const SHA256_PATTERN = /^[0-9a-f]{64}$/

export interface SyncGenerationIdentity {
    generationId: string
    manifestHash: string
    generationSequence: string
}

export type RegisteredSyncDevice = {
    deviceId: string
    status: 'active' | 'revoked' | 'forgotten'
    acknowledgedGeneration: SyncGenerationIdentity
}

export interface TombstoneCollectionPlan {
    retain: Array<{
        key: string
        deletedGenerationSequence: string
        blockingDeviceIds: string[]
    }>
    collectible: Array<{
        key: string
        deletedGenerationSequence: string
    }>
}

function compareSequences(left: string, right: string): number {
    if (left.length !== right.length) return left.length < right.length ? -1 : 1
    return left < right ? -1 : left > right ? 1 : 0
}

function validateNonemptyId(value: unknown, description: string): string {
    if (typeof value !== 'string' || value.length === 0) {
        throw new TypeError(`${description} must be a nonempty string`)
    }
    return value
}

function validateDeviceId(value: unknown): string {
    const deviceId = validateNonemptyId(value, 'Registered sync device id')
    if ([...deviceId].length > MAX_DEVICE_ID_CHARACTERS) {
        throw new TypeError('Registered sync device id must contain at most 1024 characters')
    }
    return deviceId
}

function validateGenerationIdentity(
    value: SyncGenerationIdentity,
    description: string,
): SyncGenerationIdentity {
    const generationId = validateNonemptyId(value?.generationId, `${description} generation id`)
    if (typeof value?.manifestHash !== 'string' || !SHA256_PATTERN.test(value.manifestHash)) {
        throw new TypeError(`${description} manifest hash must be a lowercase SHA-256`)
    }
    return {
        generationId,
        manifestHash: value.manifestHash,
        generationSequence: validateGenerationSequence(
            value.generationSequence,
            `${description} generation sequence`,
        ),
    }
}

function sameIdentity(left: SyncGenerationIdentity, right: SyncGenerationIdentity): boolean {
    return left.generationId === right.generationId
        && left.manifestHash === right.manifestHash
        && left.generationSequence === right.generationSequence
}

function normalizeDevices(devices: readonly RegisteredSyncDevice[]): RegisteredSyncDevice[] {
    if (!Array.isArray(devices)) throw new TypeError('Registered sync devices must be an array')
    const normalized = devices.map((device): RegisteredSyncDevice => {
        const deviceId = validateDeviceId(device?.deviceId)
        if (!['active', 'revoked', 'forgotten'].includes(device?.status)) {
            throw new TypeError('Registered sync device status is invalid')
        }
        return {
            deviceId,
            status: device.status,
            acknowledgedGeneration: validateGenerationIdentity(
                device.acknowledgedGeneration,
                'Registered sync device acknowledgement',
            ),
        }
    }).sort((left, right) => left.deviceId < right.deviceId ? -1 : left.deviceId > right.deviceId ? 1 : 0)
    for (let index = 1; index < normalized.length; index++) {
        if (normalized[index - 1].deviceId === normalized[index].deviceId) {
            throw new TypeError(`Registered sync device id is duplicate: ${normalized[index].deviceId}`)
        }
    }
    return normalized
}

export function acknowledgeDeviceGeneration(
    devices: readonly RegisteredSyncDevice[],
    deviceId: string,
    generation: SyncGenerationIdentity,
): RegisteredSyncDevice[] {
    const normalized = normalizeDevices(devices)
    const id = validateDeviceId(deviceId)
    const next = validateGenerationIdentity(generation, 'Registered sync device acknowledgement')
    const index = normalized.findIndex((device) => device.deviceId === id)
    if (index < 0) throw new TypeError(`Registered sync device is unknown: ${id}`)
    const current = normalized[index]
    if (current.status === 'revoked') {
        throw new TypeError(`Registered sync device is revoked: ${id}`)
    }
    if (current.status === 'forgotten') {
        throw new TypeError(`Registered sync device is forgotten: ${id}`)
    }
    const sequenceOrder = compareSequences(
        next.generationSequence,
        current.acknowledgedGeneration.generationSequence,
    )
    if (sequenceOrder < 0) {
        throw new TypeError(`Registered sync device acknowledgement cannot regress: ${id}`)
    }
    if (sequenceOrder === 0 && !sameIdentity(next, current.acknowledgedGeneration)) {
        throw new TypeError(`Registered sync device acknowledgement cannot fork at the same sequence: ${id}`)
    }
    if (
        next.generationId === current.acknowledgedGeneration.generationId
        && !sameIdentity(next, current.acknowledgedGeneration)
    ) {
        throw new TypeError(`Registered sync device generation id cannot change identity: ${id}`)
    }
    normalized[index] = { deviceId: id, status: 'active', acknowledgedGeneration: next }
    return normalized
}

export function revokeRegisteredDevice(
    devices: readonly RegisteredSyncDevice[],
    deviceId: string,
): RegisteredSyncDevice[] {
    const normalized = normalizeDevices(devices)
    const id = validateDeviceId(deviceId)
    const index = normalized.findIndex((device) => device.deviceId === id)
    if (index < 0) throw new TypeError(`Registered sync device is unknown: ${id}`)
    const current = normalized[index]
    if (current.status === 'forgotten') {
        throw new TypeError(`Registered sync device is forgotten: ${id}`)
    }
    normalized[index] = { ...current, status: 'revoked' }
    return normalized
}

export function forgetRegisteredDevice(
    devices: readonly RegisteredSyncDevice[],
    deviceId: string,
): RegisteredSyncDevice[] {
    const normalized = normalizeDevices(devices)
    const id = validateDeviceId(deviceId)
    const index = normalized.findIndex((device) => device.deviceId === id)
    if (index < 0) throw new TypeError(`Registered sync device is unknown: ${id}`)
    normalized[index] = { ...normalized[index], status: 'forgotten' }
    return normalized
}

export function planTombstoneCollection(input: {
    devices: readonly RegisteredSyncDevice[]
    tombstones: readonly Extract<LogicalManifestRecord, { state: 'tombstone' }>[]
}): TombstoneCollectionPlan {
    const devices = normalizeDevices(input.devices)
    if (!Array.isArray(input.tombstones)) {
        throw new TypeError('Logical tombstones must be an array')
    }
    const tombstones = input.tombstones.map((tombstone) => {
        if (tombstone?.state !== 'tombstone') {
            throw new TypeError('Logical tombstone state is invalid')
        }
        decodeLogicalRecordKey(tombstone.key)
        return {
            key: tombstone.key,
            deletedGenerationSequence: validateGenerationSequence(
                tombstone.deletedGenerationSequence,
                'Logical tombstone deleted generation',
            ),
        }
    }).sort((left, right) => left.key < right.key ? -1 : left.key > right.key ? 1 : 0)
    for (let index = 1; index < tombstones.length; index++) {
        if (tombstones[index - 1].key === tombstones[index].key) {
            throw new TypeError(`Logical tombstone key is duplicate: ${tombstones[index].key}`)
        }
    }

    const plan: TombstoneCollectionPlan = { retain: [], collectible: [] }
    for (const tombstone of tombstones) {
        const blockingDeviceIds = devices
            .filter((device) => device.status === 'revoked' || (
                device.status === 'active'
                && compareSequences(
                    device.acknowledgedGeneration.generationSequence,
                    tombstone.deletedGenerationSequence,
                ) <= 0
            ))
            .map((device) => device.deviceId)
        if (blockingDeviceIds.length > 0) {
            plan.retain.push({ ...tombstone, blockingDeviceIds })
        } else {
            plan.collectible.push(tombstone)
        }
    }
    return plan
}
