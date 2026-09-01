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
                stop: async () => ({ phase: 'idle' as const }), rotateLink: async () => ({ phase: 'running' as const }),
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
                stop: async () => ({ phase: 'idle' as const }), rotateLink: async () => ({ phase: 'running' as const }),
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
                start: async () => ({ phase: 'running' as const }), stop: async () => ({ phase: 'idle' as const }),
                rotateLink: async () => { throw new Error('http://private.example bearer secret') },
                revokeIncoming: async () => undefined, revokeOutgoing: async () => undefined,
            },
        })
        await controller.initialize()
        const preparing = controller.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })
        await expect(controller.start({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toThrow('already running')
        release?.()
        await preparing
        await expect(controller.rotateLink()).rejects.toThrow('Could not')
        expect(controller.snapshot().error).toBe('Could not update sharing.')
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
                start,
            },
            report,
        })

        expect(start).toHaveBeenCalledWith({ method: 'quick', fixedPort: 32145, publicBaseUrl: '' })
        expect(report).toHaveBeenCalledOnce()
    })
})
