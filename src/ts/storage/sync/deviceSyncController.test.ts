import { describe, expect, it, vi } from 'vitest'

import { createDeviceSyncController, startDeviceSyncAutoListen } from './deviceSyncController'

describe('device sync controller', () => {
    it('uses only incoming sources as receive targets and refreshes after a successful operation', async () => {
        const incomingSources = vi.fn(async () => [{ deviceId: 'incoming', name: 'Incoming', permissions: ['read'] as const }])
        const outgoingDevices = vi.fn(async () => [{ deviceId: 'outgoing', name: 'Outgoing', permissions: ['read'] as const }])
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources, outgoingDevices,
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
        })

        await controller.initialize()
        await controller.completeReceive(async () => undefined)

        expect(controller.snapshot().sources).toEqual([{ deviceId: 'incoming', name: 'Incoming', permissions: ['read'] }])
        expect(outgoingDevices).toHaveBeenCalledTimes(2)
        expect(incomingSources).toHaveBeenCalledTimes(2)
    })

    it('blocks receiving while the unified source is not off', async () => {
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'prepared' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
        })

        await controller.initialize()
        await expect(controller.completeReceive(async () => undefined)).rejects.toThrow('Sharing is active')
    })

    it('serializes source work and reports only a safe error', async () => {
        let release: (() => void) | undefined
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => await new Promise((resolve) => { release = () => resolve({ phase: 'prepared' as const }) }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => { throw new Error('http://private.example bearer secret') },
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
        })
        await controller.initialize()
        const preparing = controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })
        await expect(controller.start({ read: true, bidirectional: false }))
            .rejects.toThrow('already running')
        release?.()
        await preparing
        await expect(controller.rotateLink({ read: true, bidirectional: false })).rejects.toThrow('unavailable')
        expect(controller.snapshot().error).toBe('unavailable')
    })

    it('starts the saved method without surfacing an automatic startup error', async () => {
        const start = vi.fn(async () => { throw new Error('private native failure') })
        const report = vi.fn()
        await startDeviceSyncAutoListen({
            syncAutoListen: true,
            syncListenMethod: 'quick',
            syncFixedPort: 32145,
            syncPublicBaseUrl: '',
        }, {
            controller: {
                initialize: async () => undefined,
                prepare: async () => ({ phase: 'prepared' as const }),
                start,
            },
            report,
        })

        expect(start).toHaveBeenCalledWith({ read: true, bidirectional: false })
        expect(report).toHaveBeenCalledOnce()
    })

    it('stages a v2 receipt without network work, then claims it when selected', async () => {
        const claimStagedClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const joinClaimed = vi.fn()
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }), incomingSources: async () => [], outgoingDevices: async () => [],
                prepare: async () => ({ phase: 'prepared' as const }), start: async () => ({ phase: 'running' as const }),
                stop: async () => undefined, rotateLink: async () => ({ phase: 'running' as const }),
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined, claimStagedClone,
            },
            targets: {
                clone: {
                    snapshot: () => ({}), subscribe: () => () => undefined, joinClaimed,
                },
            },
        })
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)
        expect(claimStagedClone).not.toHaveBeenCalled()

        await controller.claimStagedClone()

        expect(claimStagedClone).toHaveBeenCalledOnce()
        expect(joinClaimed).toHaveBeenCalledOnce()
    })

    it('uses the staged source identity for a selected delta pull', async () => {
        const claimStagedClone = vi.fn(async () => ({ sourceDeviceId: 'source', endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'a'.repeat(64) }))
        const pullRegistered = vi.fn(async () => ({ kind: 'noChanges' }))
        const controller = createDeviceSyncController({
            facade: {
                status: async () => ({ phase: 'idle' as const }),
                incomingSources: async () => [{ deviceId: 'source', name: 'Source', permissions: ['read'] as const }],
                outgoingDevices: async () => [], prepare: async () => ({ phase: 'prepared' as const }),
                start: async () => ({ phase: 'running' as const }), stop: async () => undefined,
                rotateLink: async () => ({ phase: 'running' as const }), revokeIncoming: async () => undefined,
                revokeOutgoing: async () => undefined, claimStagedClone,
            },
            targets: { delta: { snapshot: () => ({}), subscribe: () => () => undefined, pullRegistered } },
        })
        await controller.initialize()
        controller.stageLink(`risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`)

        await controller.pullStagedDelta()

        expect(claimStagedClone).toHaveBeenCalledOnce()
        expect(pullRegistered).toHaveBeenCalledWith('source')
        expect(controller.snapshot().stagedLink).toBeNull()
    })
})
