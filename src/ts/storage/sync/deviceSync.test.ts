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
            request: { method: 'fixed-url', fixedPort: 32145, publicBaseUrl: 'https://sync.example' },
        })
    })

    it('keeps registry DTOs free of endpoint and bearer secrets while preserving source endpoint', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_sync_incoming_sources') {
                return [{ deviceId: 'source', name: 'Source', permissions: ['read'], endpoint: 'http://private', bearer: 'secret' }]
            }
            return { phase: 'running', endpoint: 'http://private', bearer: 'secret', pairingUri: 'risuailocal://peer-clone/v2' }
        })
        const facade = createDeviceSyncFacade({ invoke })

        const sources = await facade.incomingSources()
        const status = await facade.status()

        expect(sources).toEqual([{ deviceId: 'source', name: 'Source', permissions: ['read'] }])
        expect(status).toEqual({
            phase: 'running', endpoint: 'http://private', pairingUri: 'risuailocal://peer-clone/v2',
        })
    })

    it('uses registered commands without accepting a bearer argument', async () => {
        const invoke = vi.fn(async () => ({
            sourceDeviceId: '223e4567-e89b-42d3-a456-426614174000',
            endpoint: 'http://current/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'a'.repeat(64),
        }))
        const facade = createDeviceSyncFacade({ invoke })

        await facade.claimStagedClone({
            endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'manifest', claim: 'claim',
        })
        await facade.reconnectRegisteredClone('223e4567-e89b-42d3-a456-426614174000')
        expect(invoke.mock.calls).toEqual([
            ['peer_clone_claim_client', { endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'manifest', claim: 'claim' }],
            ['peer_clone_claim_registered_client', { deviceId: '223e4567-e89b-42d3-a456-426614174000' }],
        ])
    })

    it('rejects malformed or secret-bearing native claim descriptors', async () => {
        const values = [
            { sourceDeviceId: 'source', endpoint: 'http://current/', sessionId: 'session' },
            { sourceDeviceId: null, endpoint: 'http://current/', sessionId: 'session', manifestId: 'manifest' },
            { sourceDeviceId: 'source', endpoint: 'http://current/', sessionId: 'session', manifestId: 'manifest', bearer: 'secret' },
            {
                sourceDeviceId: '223e4567-e89b-42d3-a456-426614174000',
                endpoint: 'http://user:secret@current/',
                sessionId: '123e4567-e89b-12d3-a456-426614174000',
                manifestId: 'a'.repeat(64),
            },
        ]
        const invoke = vi.fn(async () => values.shift())
        const facade = createDeviceSyncFacade({ invoke })
        const link = { endpoint: 'http://source/', sessionId: 'session', manifestId: 'manifest', claim: 'claim' }

        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
    })

    it('drops non-finite source expiration values', async () => {
        const invoke = vi.fn(async () => ({ phase: 'running', expiresAtMs: Number.NaN }))
        const facade = createDeviceSyncFacade({ invoke })

        await expect(facade.status()).resolves.toEqual({ phase: 'running' })
    })

    it('preserves the transient native preparing source phase', async () => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => ({ phase: 'preparing' })) })

        await expect(facade.status()).resolves.toEqual({ phase: 'preparing' })
    })

    it('accepts only exact native source error categories from latestError', async () => {
        const invoke = vi.fn()
            .mockResolvedValueOnce({ phase: 'error', latestError: 'cleanup-failed' })
            .mockResolvedValueOnce({ phase: 'error', latestError: 'private native details' })
        const facade = createDeviceSyncFacade({ invoke })

        await expect(facade.status()).resolves.toEqual({ phase: 'error', latestError: 'cleanup-failed' })
        await expect(facade.status()).resolves.toEqual({ phase: 'error' })
    })

    it.each([
        ['authorizationExpired', 'registration-expired'],
        ['sourceMissing', 'registration-expired'],
        ['identityMismatch', 'registration-expired'],
        ['transportUnavailable', 'transport-changed'],
        ['permissionDenied', 'operation-failed'],
        ['laneUnavailable', 'operation-failed'],
        ['private native detail', 'operation-failed'],
    ])('maps registered rejection %s to safe category %s', async (nativeError, category) => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => { throw new Error(nativeError) }) })

        await expect(facade.reconnectRegisteredClone('source')).rejects.toMatchObject({ code: category })
    })

    it('keeps directional registry revoke commands separate', async () => {
        const invoke = vi.fn(async () => undefined)
        const facade = createDeviceSyncFacade({ invoke })

        await facade.revokeOutgoing('device')
        await facade.revokeIncoming('source')

        expect(invoke.mock.calls).toEqual([
            ['peer_sync_revoke_outgoing_device', { deviceId: 'device' }],
            ['peer_sync_remove_incoming_source', { deviceId: 'source' }],
        ])
    })
})
