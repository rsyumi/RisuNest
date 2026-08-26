import { decodeLogicalRecordKey } from './logicalRecordKey'
import {
    validateGenerationSequence,
    type LogicalManifestRecord,
} from './logicalManifest'

const MAX_DEVICE_ID_BYTES = 1024
const textEncoder = new TextEncoder()

export type RegisteredSyncDevice =
    | {
          deviceId: string
          status: 'active'
          acknowledgedGenerationSequence: string
      }
    | {
          deviceId: string
          status: 'forgotten'
          forgottenGenerationSequence: string
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

function validateDeviceId(value: unknown): string {
    if (
        typeof value !== 'string'
        || value.length === 0
        || textEncoder.encode(value).byteLength > MAX_DEVICE_ID_BYTES
    ) {
        throw new TypeError('Registered sync device id must be a bounded nonempty string')
    }
    return value
}

function normalizeDevices(devices: readonly RegisteredSyncDevice[]): RegisteredSyncDevice[] {
    if (!Array.isArray(devices)) throw new TypeError('Registered sync devices must be an array')
    const normalized = devices.map((device): RegisteredSyncDevice => {
        const deviceId = validateDeviceId(device?.deviceId)
        if (device?.status === 'active') {
            return {
                deviceId,
                status: 'active',
                acknowledgedGenerationSequence: validateGenerationSequence(
                    device.acknowledgedGenerationSequence,
                    'Registered sync device acknowledgement',
                ),
            }
        }
        if (device?.status === 'forgotten') {
            return {
                deviceId,
                status: 'forgotten',
                forgottenGenerationSequence: validateGenerationSequence(
                    device.forgottenGenerationSequence,
                    'Registered sync device forgotten generation',
                ),
            }
        }
        throw new TypeError('Registered sync device status is invalid')
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
    generationSequence: string,
): RegisteredSyncDevice[] {
    const normalized = normalizeDevices(devices)
    const id = validateDeviceId(deviceId)
    const nextSequence = validateGenerationSequence(
        generationSequence,
        'Registered sync device acknowledgement',
    )
    const index = normalized.findIndex((device) => device.deviceId === id)
    if (index < 0) throw new TypeError(`Registered sync device is unknown: ${id}`)
    const current = normalized[index]
    if (current.status === 'forgotten') {
        throw new TypeError(`Registered sync device is forgotten: ${id}`)
    }
    if (compareSequences(nextSequence, current.acknowledgedGenerationSequence) < 0) {
        throw new TypeError(`Registered sync device acknowledgement cannot regress: ${id}`)
    }
    normalized[index] = {
        deviceId: id,
        status: 'active',
        acknowledgedGenerationSequence: nextSequence,
    }
    return normalized
}

export function forgetRegisteredDevice(
    devices: readonly RegisteredSyncDevice[],
    deviceId: string,
    forgottenGenerationSequence: string,
): RegisteredSyncDevice[] {
    const normalized = normalizeDevices(devices)
    const id = validateDeviceId(deviceId)
    const forgottenAt = validateGenerationSequence(
        forgottenGenerationSequence,
        'Registered sync device forgotten generation',
    )
    const index = normalized.findIndex((device) => device.deviceId === id)
    if (index < 0) throw new TypeError(`Registered sync device is unknown: ${id}`)
    const current = normalized[index]
    if (
        current.status === 'active'
        && compareSequences(forgottenAt, current.acknowledgedGenerationSequence) < 0
    ) {
        throw new TypeError(`Registered sync device forgotten generation predates acknowledgement: ${id}`)
    }
    if (
        current.status === 'forgotten'
        && compareSequences(forgottenAt, current.forgottenGenerationSequence) < 0
    ) {
        throw new TypeError(`Registered sync device forgotten generation cannot regress: ${id}`)
    }
    normalized[index] = {
        deviceId: id,
        status: 'forgotten',
        forgottenGenerationSequence: forgottenAt,
    }
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
            .filter((device) => device.status === 'active')
            .filter((device) => compareSequences(
                device.acknowledgedGenerationSequence,
                tombstone.deletedGenerationSequence,
            ) <= 0)
            .map((device) => device.deviceId)
        if (blockingDeviceIds.length > 0) {
            plan.retain.push({ ...tombstone, blockingDeviceIds })
        } else {
            plan.collectible.push(tombstone)
        }
    }
    return plan
}
