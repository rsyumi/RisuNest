import { describe, expect, it, vi } from 'vitest'

import { createDeviceSyncFacade, parseDeviceSyncUri } from './deviceSync'

const runtimeStub = () => {
    const fence = {
        refreshCommittedWorkingSet: vi.fn(async (_revision: number) => undefined),
        release: vi.fn(),
    }
    return {
        fence,
        flushPendingData: vi.fn(async (_reason: string) => undefined),
        capturePersistentMutationToken: vi.fn(async (_reason: string) => ({ revision: 1, mutationGeneration: 1 })),
        acquireDestructiveReplacementFence: vi.fn(async (_token: {
            revision: number
            mutationGeneration: number
        }) => fence),
    }
}

const canonicalOperationId = '00000000-0000-4000-8000-0000000000a1'

describe('device sync facade', () => {
    it('forwards saved sharing settings and rejects a fixed port of zero', async () => {
        const invoke = vi.fn(async () => ({ phase: 'prepared' }))
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        await facade.prepare({ method: 'fixed-url', fixedPort: 32145, publicBaseUrl: 'https://sync.example' })
        await expect(facade.prepare({ method: 'lan', fixedPort: 0, publicBaseUrl: '' }))
            .rejects.toThrow('valid port')

        expect(invoke).toHaveBeenCalledWith('device_sync_prepare', {
            request: { method: 'fixed-url', fixedPort: 32145, publicBaseUrl: 'https://sync.example' },
        })
    })

    it('keeps registry DTOs free of endpoint and bearer secrets while preserving a valid source endpoint', async () => {
        const endpoint = 'http://192.168.1.2:32145/'
        const pairingUri = `risuailocal://peer-clone/v2?endpoint=${encodeURIComponent(endpoint)}&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_sync_incoming_sources') {
                return [{ deviceId: 'source', name: 'Source', permissions: ['read'], endpoint: 'http://private', bearer: 'secret' }]
            }
            return { phase: 'running', endpoint, bearer: 'secret', pairingUri }
        })
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        const sources = await facade.incomingSources()
        const status = await facade.status()

        expect(sources).toEqual([{ deviceId: 'source', name: 'Source', permissions: ['read'] }])
        expect(status).toEqual({
            phase: 'running', endpoint, pairingUri,
        })
    })

    it('uses registered commands without accepting a bearer argument', async () => {
        const invoke = vi.fn(async () => ({
            sourceDeviceId: '223e4567-e89b-42d3-a456-426614174000',
            endpoint: 'http://current/',
            sessionId: '123e4567-e89b-12d3-a456-426614174000',
            manifestId: 'a'.repeat(64),
        }))
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        await facade.claimStagedClone({
            endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'manifest', claim: 'claim',
        })
        await facade.reconnectRegisteredClone('223e4567-e89b-42d3-a456-426614174000')
        expect(invoke.mock.calls).toEqual([
            ['peer_clone_claim_v2_client', { endpoint: 'http://192.168.1.2:32145/', sessionId: 'session', manifestId: 'manifest', claim: 'claim' }],
            ['peer_clone_claim_registered_client', { deviceId: '223e4567-e89b-42d3-a456-426614174000' }],
        ])
    })

    it.each([
        ['peer-clone', '/v1'],
        ['peer-delta', '/v1'],
        ['peer-sync', '/v1'],
        ['peer-clone', '/v3'],
    ])('rejects the %s lane link at %s instead of a device sync v2 link', (host, path) => {
        const uri = `risuailocal://${host}${path}?endpoint=${encodeURIComponent('http://10.1.2.3:32145')}&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`

        expect(() => parseDeviceSyncUri(uri)).toThrow('Invalid device sync link')
    })

    it.each([
        'http://127.0.0.1:32145',
        'http://127.1:32145',
        'http://127.255.255.254:32145',
        'http://[::1]:32145',
    ])('rejects canonical v2 loopback endpoint %s', (endpoint) => {
        const pairingUri = `risuailocal://peer-clone/v2?endpoint=${encodeURIComponent(endpoint)}&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`

        expect(() => parseDeviceSyncUri(pairingUri)).toThrow('Invalid device sync link')
    })

    it.each([
        ['http://10.1.2.3:32145', 'http://10.1.2.3:32145/'],
        ['http://169.254.1.2:32145', 'http://169.254.1.2:32145/'],
        ['http://[fd12:3456::1]:32145', 'http://[fd12:3456::1]:32145/'],
        ['http://[fe80::1234]:32145', 'http://[fe80::1234]:32145/'],
        ['https://sync.example.com', 'https://sync.example.com/'],
    ])('accepts canonical v2 endpoint %s under the LAN/public HTTPS policy', (endpoint, canonical) => {
        const pairingUri = `risuailocal://peer-clone/v2?endpoint=${encodeURIComponent(endpoint)}&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`

        expect(parseDeviceSyncUri(pairingUri).endpoint).toBe(canonical)
    })

    const session = 'session=123e4567-e89b-12d3-a456-426614174000'
    const canonicalQuery = `endpoint=${encodeURIComponent('http://10.1.2.3:32145')}&${session}&manifest=${'a'.repeat(64)}`

    it.each([
        ['a non-hex claim fragment', `risuailocal://peer-clone/v2?${canonicalQuery}#claim=${'g'.repeat(64)}`],
        ['a short claim fragment', `risuailocal://peer-clone/v2?${canonicalQuery}#claim=${'b'.repeat(63)}`],
        ['a duplicated session key', `risuailocal://peer-clone/v2?${canonicalQuery}&${session}#claim=${'b'.repeat(64)}`],
        ['an extra unknown key', `risuailocal://peer-clone/v2?${canonicalQuery}&bearer=secret#claim=${'b'.repeat(64)}`],
        ['a missing manifest key', `risuailocal://peer-clone/v2?endpoint=${encodeURIComponent('http://10.1.2.3:32145')}&${session}#claim=${'b'.repeat(64)}`],
        ['a body past the 8192 character pairing URI ceiling', `risuailocal://peer-clone/v2?endpoint=${encodeURIComponent(`http://10.1.2.3:32145/${'a'.repeat(8192)}`)}&${session}&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`],
    ])('rejects a canonical v2 link carrying %s', (_reason, uri) => {
        expect(() => parseDeviceSyncUri(uri)).toThrow('Invalid device sync link')
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
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })
        const link = { endpoint: 'http://source/', sessionId: 'session', manifestId: 'manifest', claim: 'claim' }

        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
        await expect(facade.claimStagedClone(link)).rejects.toMatchObject({ code: 'unavailable' })
    })

    it('rejects non-finite source expiration values', async () => {
        const invoke = vi.fn(async () => ({ phase: 'running', expiresAtMs: Number.NaN }))
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
    })

    it.each([undefined, 'unknown'])('rejects missing or unknown source phase %s', async (phase) => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => ({ phase })), runtime: runtimeStub() })

        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
    })

    it.each([
        'http://user:secret@192.168.1.2:32145/',
        'http://example.com:32145/',
        'http://127.0.0.1:32145/',
        'https://192.168.1.2/',
        'https://public.example:8443/',
    ])('rejects unsafe source endpoint %s', async (endpoint) => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => ({ phase: 'running', endpoint })), runtime: runtimeStub() })

        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
    })

    it('rejects malformed or endpoint-mismatched source pairing state', async () => {
        const endpoint = 'http://192.168.1.2:32145/'
        const otherEndpoint = 'http://192.168.1.3:32145/'
        const validPairing = `risuailocal://peer-clone/v2?endpoint=${encodeURIComponent(otherEndpoint)}&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
        const matchingPairing = `risuailocal://peer-clone/v2?endpoint=${encodeURIComponent(endpoint)}&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
        const invoke = vi.fn()
            .mockResolvedValueOnce({ phase: 'running', pairingUri: validPairing })
            .mockResolvedValueOnce({ phase: 'running', endpoint, pairingUri: 'risuailocal://peer-clone/v2' })
            .mockResolvedValueOnce({ phase: 'running', endpoint, pairingUri: validPairing })
            .mockResolvedValueOnce({
                phase: 'running', endpoint,
                pairingUri: matchingPairing.replace('risuailocal://', 'risuailocal://user:secret@'),
            })
            .mockResolvedValueOnce({ phase: 'running', endpoint, pairingUri: `\n${matchingPairing}\t` })
            .mockResolvedValueOnce({
                phase: 'running', endpoint,
                pairingUri: matchingPairing.replace(
                    `endpoint=${encodeURIComponent(endpoint)}&session=123e4567-e89b-12d3-a456-426614174000`,
                    `session=123e4567-e89b-12d3-a456-426614174000&endpoint=${encodeURIComponent(endpoint)}`,
                ),
            })
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
    })

    it('preserves the transient native preparing source phase', async () => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => ({ phase: 'preparing' })), runtime: runtimeStub() })

        await expect(facade.status()).resolves.toEqual({ phase: 'preparing' })
    })

    it('treats native null option fields as absent', async () => {
        const facade = createDeviceSyncFacade({
            invoke: vi.fn(async () => ({
                phase: 'idle', endpoint: null, pairingUri: null, expiresAtMs: null, latestError: null,
            })),
            runtime: runtimeStub(),
        })

        await expect(facade.status()).resolves.toEqual({ phase: 'idle' })
    })

    it('accepts only exact native source error categories from latestError', async () => {
        const invoke = vi.fn()
            .mockResolvedValueOnce({ phase: 'error', latestError: 'cleanup-failed' })
            .mockResolvedValueOnce({ phase: 'error', latestError: 'private native details' })
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        await expect(facade.status()).resolves.toEqual({ phase: 'error', latestError: 'cleanup-failed' })
        await expect(facade.status()).resolves.toEqual({ phase: 'error' })
    })

    it.each([
        ['registrationBlockedByActiveWork', 'registration-blocked-by-active-work'],
        ['sourceInUse', 'source-in-use'],
        ['sourceChanged', 'source-changed'],
        ['deltaCompletionRetained', 'delta-completion-retained'],
        ['peerOutdated', 'peer-outdated'],
        ['authorizationExpired', 'registration-expired'],
        ['sourceMissing', 'registration-expired'],
        ['identityMismatch', 'transport-changed'],
        ['transportUnavailable', 'transport-unavailable'],
        ['permissionDenied', 'operation-failed'],
        ['laneUnavailable', 'operation-failed'],
        ['operationFailed', 'operation-failed'],
        ['invalid-configuration', 'invalid-configuration'],
        ['port-unavailable', 'port-unavailable'],
        ['preparation-failed', 'preparation-failed'],
        ['transport-unavailable', 'transport-unavailable'],
        ['cleanup-failed', 'cleanup-failed'],
        ['state-unavailable', 'state-unavailable'],
        ['private native detail', 'operation-failed'],
    ])('maps bounded native code %s to safe category %s', async (nativeError, category) => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => { throw new Error(nativeError) }), runtime: runtimeStub() })

        await expect(facade.reconnectRegisteredClone('source')).rejects.toMatchObject({ code: category })
    })

    it.each([
        'peer-registration-blocked-by-active-work',
        'Validation("peer-registration-blocked-by-active-work")',
        'registrationBlockedByActiveWork detail',
        'peer-source-in-use',
        'toString',
        'constructor',
    ])('no longer gives the unbounded native string %s a category of its own', async (nativeError) => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => { throw new Error(nativeError) }), runtime: runtimeStub() })

        await expect(facade.claimStagedClone({
            endpoint: 'http://10.1.2.3:32145',
            sessionId: '00000000-0000-4000-8000-000000000001',
            manifestId: 'a'.repeat(64),
            claim: 'b'.repeat(64),
        })).rejects.toMatchObject({ code: 'operation-failed' })
    })

    it('keeps the refusal category for the bounded registration code', async () => {
        const facade = createDeviceSyncFacade({ invoke: vi.fn(async () => { throw new Error('registrationBlockedByActiveWork') }), runtime: runtimeStub() })

        await expect(facade.claimStagedClone({
            endpoint: 'http://10.1.2.3:32145',
            sessionId: '00000000-0000-4000-8000-000000000001',
            manifestId: 'a'.repeat(64),
            claim: 'b'.repeat(64),
        })).rejects.toMatchObject({ code: 'registration-blocked-by-active-work' })
    })

    it('treats an absent last remote commit as an unset field', async () => {
        const invoke = vi.fn()
            .mockResolvedValueOnce({ phase: 'idle' })
            .mockResolvedValueOnce({ phase: 'idle', lastRemoteCommit: null })
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        await expect(facade.status()).resolves.toEqual({ phase: 'idle' })
        await expect(facade.status()).resolves.toEqual({ phase: 'idle' })
    })

    it('keeps a well-formed last remote commit', async () => {
        const lastRemoteCommit = { operationId: canonicalOperationId, committedRevision: 12 }
        const facade = createDeviceSyncFacade({
            invoke: vi.fn(async () => ({ phase: 'prepared', lastRemoteCommit })),
            runtime: runtimeStub(),
        })

        await expect(facade.status()).resolves.toEqual({ phase: 'prepared', lastRemoteCommit })
    })

    it.each([
        ['an extra key', { operationId: canonicalOperationId, committedRevision: 12, extra: 'secret' }],
        ['a missing revision', { operationId: canonicalOperationId }],
        ['a non-UUID operation', { operationId: 'not-a-uuid', committedRevision: 12 }],
        ['a negative revision', { operationId: canonicalOperationId, committedRevision: -1 }],
        ['a fractional revision', { operationId: canonicalOperationId, committedRevision: 1.5 }],
        ['a string revision', { operationId: canonicalOperationId, committedRevision: '12' }],
        ['an array', [canonicalOperationId, 12]],
    ])('rejects a last remote commit with %s', async (_label, lastRemoteCommit) => {
        const facade = createDeviceSyncFacade({
            invoke: vi.fn(async () => ({ phase: 'prepared', lastRemoteCommit })),
            runtime: runtimeStub(),
        })

        await expect(facade.status()).rejects.toMatchObject({ code: 'state-unavailable' })
    })

    it('awaits the persistent-data flush before native preparation', async () => {
        const events: string[] = []
        let finishFlush: (() => void) | undefined
        const runtime = runtimeStub()
        runtime.flushPendingData.mockImplementation(async () => {
            events.push('flush-start')
            await new Promise<void>((resolve) => { finishFlush = resolve })
            events.push('flush-finish')
        })
        const invoke = vi.fn(async (command: string) => {
            events.push(command)
            return { phase: 'prepared' }
        })
        const facade = createDeviceSyncFacade({ invoke, runtime })

        const preparing = facade.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' })
        await vi.waitFor(() => expect(events).toEqual(['flush-start']))
        expect(invoke).not.toHaveBeenCalled()

        finishFlush?.()
        await preparing

        expect(runtime.flushPendingData).toHaveBeenCalledWith('device-sync-source-prepare')
        expect(runtime.flushPendingData).toHaveBeenCalledOnce()
        expect(events).toEqual(['flush-start', 'flush-finish', 'device_sync_prepare'])
    })

    it('does not invoke native preparation when the flush is refused', async () => {
        const failure = new Error('flush failed')
        const runtime = runtimeStub()
        runtime.flushPendingData.mockRejectedValue(failure)
        const invoke = vi.fn(async () => ({ phase: 'prepared' }))
        const facade = createDeviceSyncFacade({ invoke, runtime })

        await expect(facade.prepare({ method: 'lan', fixedPort: 32145, publicBaseUrl: '' }))
            .rejects.toBe(failure)
        expect(invoke).not.toHaveBeenCalled()
    })

    it('validates the settings before spending a flush', async () => {
        const runtime = runtimeStub()
        const invoke = vi.fn(async () => ({ phase: 'prepared' }))
        const facade = createDeviceSyncFacade({ invoke, runtime })

        await expect(facade.prepare({ method: 'lan', fixedPort: 0, publicBaseUrl: '' }))
            .rejects.toThrow('valid port')
        expect(runtime.flushPendingData).not.toHaveBeenCalled()
        expect(invoke).not.toHaveBeenCalled()
    })

    it('refreshes the working set to the committed revision after a remote commit', async () => {
        const order: string[] = []
        const runtime = runtimeStub()
        runtime.flushPendingData.mockImplementation(async () => { order.push('flush') })
        runtime.capturePersistentMutationToken.mockImplementation(async () => {
            order.push('token')
            return { revision: 4, mutationGeneration: 2 }
        })
        runtime.acquireDestructiveReplacementFence.mockImplementation(async () => {
            order.push('fence')
            return runtime.fence
        })
        runtime.fence.refreshCommittedWorkingSet.mockImplementation(async () => { order.push('refresh') })
        runtime.fence.release.mockImplementation(() => { order.push('release') })
        const facade = createDeviceSyncFacade({ invoke: vi.fn(), runtime })

        await expect(facade.refreshAfterRemoteCommit({
            operationId: canonicalOperationId, committedRevision: 12,
        })).resolves.toEqual({ discardedPendingEdits: false })

        expect(order).toEqual(['flush', 'token', 'fence', 'refresh', 'release'])
        expect(runtime.flushPendingData).toHaveBeenCalledWith('device-sync-remote-commit')
        expect(runtime.capturePersistentMutationToken).toHaveBeenCalledWith('device-sync-remote-commit')
        expect(runtime.acquireDestructiveReplacementFence).toHaveBeenCalledWith({ revision: 4, mutationGeneration: 2 })
        expect(runtime.fence.refreshCommittedWorkingSet).toHaveBeenCalledWith(12)
    })

    it('still refreshes when the flush conflicts with the peer commit', async () => {
        const runtime = runtimeStub()
        runtime.flushPendingData.mockRejectedValue(new Error('revision conflict'))
        const facade = createDeviceSyncFacade({ invoke: vi.fn(), runtime })

        await expect(facade.refreshAfterRemoteCommit({
            operationId: canonicalOperationId, committedRevision: 12,
        })).resolves.toEqual({ discardedPendingEdits: true })

        expect(runtime.fence.refreshCommittedWorkingSet).toHaveBeenCalledWith(12)
        expect(runtime.fence.release).toHaveBeenCalledOnce()
    })

    it('releases the fence and reports a bounded code when the refresh fails', async () => {
        const runtime = runtimeStub()
        runtime.fence.refreshCommittedWorkingSet.mockRejectedValue(new Error('operationFailed'))
        const facade = createDeviceSyncFacade({ invoke: vi.fn(), runtime })

        await expect(facade.refreshAfterRemoteCommit({
            operationId: canonicalOperationId, committedRevision: 12,
        })).rejects.toMatchObject({ code: 'operation-failed' })

        expect(runtime.fence.release).toHaveBeenCalledOnce()
    })

    it('keeps directional registry revoke commands separate', async () => {
        const invoke = vi.fn(async () => undefined)
        const facade = createDeviceSyncFacade({ invoke, runtime: runtimeStub() })

        await facade.revokeOutgoing('device')
        await facade.revokeIncoming('source')

        expect(invoke.mock.calls).toEqual([
            ['peer_sync_revoke_outgoing_device', { deviceId: 'device' }],
            ['peer_sync_remove_incoming_source', { deviceId: 'source' }],
        ])
    })
})
