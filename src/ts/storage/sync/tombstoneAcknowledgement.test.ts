import { describe, expect, it } from 'vitest'

import { encodeLogicalRecordKey } from './logicalRecordKey'
import {
    acknowledgeDeviceGeneration,
    forgetRegisteredDevice,
    planTombstoneCollection,
    revokeRegisteredDevice,
    type RegisteredSyncDevice,
    type SyncGenerationIdentity,
} from './tombstoneAcknowledgement'

const tombstoneKey = encodeLogicalRecordKey({ kind: 'asset', logicalKey: 'old-asset' })
const hashA = 'a'.repeat(64)
const hashB = 'b'.repeat(64)

function generation(
    generationSequence: string,
    generationId = `generation-${generationSequence}`,
    manifestHash = hashA,
): SyncGenerationIdentity {
    return { generationId, manifestHash, generationSequence }
}

const devices: RegisteredSyncDevice[] = [
    { deviceId: 'desktop-a', status: 'active', acknowledgedGeneration: generation('6') },
    { deviceId: 'phone-a', status: 'active', acknowledgedGeneration: generation('5') },
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

    it('makes a tombstone collectible after an exact acknowledgement advances', () => {
        const acknowledged = acknowledgeDeviceGeneration(devices, 'phone-a', generation('7'))

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
        expect(() => acknowledgeDeviceGeneration(acknowledged, 'phone-a', generation('6')))
            .toThrow('regress')
    })

    it('accepts an exact acknowledgement retry but rejects a same-sequence fork', () => {
        expect(acknowledgeDeviceGeneration(devices, 'phone-a', generation('5'))).toEqual(devices)

        expect(() => acknowledgeDeviceGeneration(
            devices,
            'phone-a',
            generation('5', 'forked-generation'),
        )).toThrow('same sequence')
        expect(() => acknowledgeDeviceGeneration(
            devices,
            'phone-a',
            generation('5', 'generation-5', hashB),
        )).toThrow('same sequence')
        expect(() => acknowledgeDeviceGeneration(
            devices,
            'phone-a',
            generation('6', 'generation-5', hashB),
        )).toThrow('generation id')
    })

    it('makes a revoked device an unconditional blocker and rejects later acknowledgements', () => {
        const revoked = revokeRegisteredDevice(devices, 'desktop-a')

        expect(revoked).toContainEqual({
            deviceId: 'desktop-a',
            status: 'revoked',
            acknowledgedGeneration: generation('6'),
        })
        expect(planTombstoneCollection({
            devices: revoked,
            tombstones: [{
                key: tombstoneKey,
                state: 'tombstone',
                deletedGenerationSequence: '999',
            }],
        }).retain[0].blockingDeviceIds).toContain('desktop-a')
        expect(() => acknowledgeDeviceGeneration(revoked, 'desktop-a', generation('7')))
            .toThrow('revoked')
    })

    it('removes a forgotten device from blockers while retaining its exact last acknowledgement', () => {
        const forgotten = forgetRegisteredDevice(
            revokeRegisteredDevice(devices, 'phone-a'),
            'phone-a',
        )

        expect(forgotten).toContainEqual({
            deviceId: 'phone-a',
            status: 'forgotten',
            acknowledgedGeneration: generation('5'),
        })
        expect(planTombstoneCollection({
            devices: forgotten,
            tombstones: [{
                key: tombstoneKey,
                state: 'tombstone',
                deletedGenerationSequence: '5',
            }],
        }).collectible).toHaveLength(1)
        expect(() => acknowledgeDeviceGeneration(forgotten, 'phone-a', generation('9')))
            .toThrow('forgotten')
        expect(() => revokeRegisteredDevice(forgotten, 'phone-a')).toThrow('forgotten')
    })

    it('rejects malformed exact identities and duplicate registered device identities', () => {
        expect(() => planTombstoneCollection({
            devices: [devices[0], devices[0]],
            tombstones: [],
        })).toThrow('duplicate')
        expect(() => acknowledgeDeviceGeneration(devices, 'phone-a', {
            generationId: 'generation-7',
            manifestHash: 'ABC',
            generationSequence: '7',
        })).toThrow('lowercase SHA-256')
    })

    it('counts device limits as Unicode characters and leaves generation ids unbounded', () => {
        const longGenerationId = 'generation'.repeat(1_025)
        const acceptedDeviceId = '🐿'.repeat(1_024)
        const accepted: RegisteredSyncDevice[] = [{
            deviceId: acceptedDeviceId,
            status: 'active',
            acknowledgedGeneration: generation('1', longGenerationId),
        }]

        expect(planTombstoneCollection({ devices: accepted, tombstones: [] })).toEqual({
            retain: [],
            collectible: [],
        })
        expect(() => planTombstoneCollection({
            devices: [{ ...accepted[0], deviceId: `${acceptedDeviceId}🐿` }],
            tombstones: [],
        })).toThrow('device id')
    })
})
