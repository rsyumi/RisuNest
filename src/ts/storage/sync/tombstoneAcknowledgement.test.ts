import { describe, expect, it } from 'vitest'

import { encodeLogicalRecordKey } from './logicalRecordKey'
import {
    acknowledgeDeviceGeneration,
    forgetRegisteredDevice,
    planTombstoneCollection,
    type RegisteredSyncDevice,
} from './tombstoneAcknowledgement'

const tombstoneKey = encodeLogicalRecordKey({ kind: 'asset', logicalKey: 'old-asset' })

const devices: RegisteredSyncDevice[] = [
    { deviceId: 'desktop-a', status: 'active', acknowledgedGenerationSequence: '6' },
    { deviceId: 'phone-a', status: 'active', acknowledgedGenerationSequence: '5' },
]

describe('tombstone acknowledgement', () => {
    it('retains a tombstone until every active device acknowledges a later generation', () => {
        expect(planTombstoneCollection({
            devices,
            tombstones: [{
                key: tombstoneKey,
                state: 'tombstone',
                deletedGenerationSequence: '5',
            }],
        })).toEqual({
            retain: [{
                key: tombstoneKey,
                deletedGenerationSequence: '5',
                blockingDeviceIds: ['phone-a'],
            }],
            collectible: [],
        })
    })

    it('makes a tombstone collectible after acknowledgement advances monotonically', () => {
        const acknowledged = acknowledgeDeviceGeneration(devices, 'phone-a', '7')

        expect(planTombstoneCollection({
            devices: acknowledged,
            tombstones: [{
                key: tombstoneKey,
                state: 'tombstone',
                deletedGenerationSequence: '5',
            }],
        }).collectible).toEqual([{
            key: tombstoneKey,
            deletedGenerationSequence: '5',
        }])
        expect(() => acknowledgeDeviceGeneration(acknowledged, 'phone-a', '6'))
            .toThrow('regress')
    })

    it('removes a forgotten device from acknowledgement blockers without deleting its registry entry', () => {
        const forgotten = forgetRegisteredDevice(devices, 'phone-a', '8')

        expect(forgotten).toContainEqual({
            deviceId: 'phone-a',
            status: 'forgotten',
            forgottenGenerationSequence: '8',
        })
        expect(planTombstoneCollection({
            devices: forgotten,
            tombstones: [{
                key: tombstoneKey,
                state: 'tombstone',
                deletedGenerationSequence: '5',
            }],
        }).collectible).toHaveLength(1)
        expect(() => acknowledgeDeviceGeneration(forgotten, 'phone-a', '9'))
            .toThrow('forgotten')
    })

    it('rejects duplicate registered device identities', () => {
        expect(() => planTombstoneCollection({
            devices: [devices[0], devices[0]],
            tombstones: [],
        })).toThrow('duplicate')
    })
})
