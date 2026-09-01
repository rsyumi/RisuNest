import { describe, expect, it, vi } from 'vitest'

import { createDeviceSyncFacade } from './deviceSync'

describe('device sync facade', () => {
    it('forwards saved sharing settings and rejects a fixed port of zero', async () => {
        const invoke = vi.fn(async () => ({ phase: 'prepared' }))
        const facade = createDeviceSyncFacade({ invoke })

        await facade.prepare({ method: 'fixed-url', fixedPort: 32145, publicBaseUrl: 'https://sync.example' })
        await expect(facade.prepare({ method: 'lan', fixedPort: 0, publicBaseUrl: '' }))
            .rejects.toThrow('valid port')

        expect(invoke).toHaveBeenCalledWith('device_sync_prepare', {
            method: 'fixed-url', fixedPort: 32145, publicBaseUrl: 'https://sync.example',
        })
    })

    it('keeps registry and status DTOs free of endpoint and bearer secrets', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'device_sync_registered_sources') {
                return [{ deviceId: 'source', name: 'Source', permissions: ['read'], endpoint: 'http://private', bearer: 'secret' }]
            }
            return { phase: 'running', endpoint: 'http://private', bearer: 'secret', pairingUri: 'risuailocal://peer-clone/v2' }
        })
        const facade = createDeviceSyncFacade({ invoke })

        const sources = await facade.incomingSources()
        const status = await facade.status()

        expect(sources).toEqual([{ deviceId: 'source', name: 'Source', permissions: ['read'] }])
        expect(status).toEqual({ phase: 'running', pairingUri: 'risuailocal://peer-clone/v2' })
    })

    it('uses registered commands without accepting a bearer argument', async () => {
        const invoke = vi.fn(async () => ({ endpoint: 'http://current', sessionId: 'session', manifestId: 'manifest' }))
        const facade = createDeviceSyncFacade({ invoke })

        await facade.claimRegisteredClone('source')
        await facade.pullRegisteredDelta('source')
        await facade.syncRegisteredBidirectional('source')
        await facade.resolveRegisteredBidirectional('source', 'operation', 'local')

        expect(invoke.mock.calls).toEqual([
            ['peer_clone_claim_registered_client', { deviceId: 'source' }],
            ['peer_delta_pull_registered', { deviceId: 'source' }],
            ['peer_bidirectional_sync_registered', { deviceId: 'source' }],
            ['peer_bidirectional_resolve_registered', { deviceId: 'source', operationId: 'operation', winner: 'local' }],
        ])
    })

    it('keeps directional registry revoke commands separate', async () => {
        const invoke = vi.fn(async () => undefined)
        const facade = createDeviceSyncFacade({ invoke })

        await facade.revokeOutgoing('device')
        await facade.revokeIncoming('source')

        expect(invoke.mock.calls).toEqual([
            ['device_sync_revoke_device', { deviceId: 'device' }],
            ['device_sync_revoke_source', { deviceId: 'source' }],
        ])
    })
})
