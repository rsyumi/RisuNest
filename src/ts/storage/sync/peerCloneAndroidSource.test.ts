import { describe, expect, it, vi } from 'vitest'

import { createAndroidPeerCloneSourceFacade } from './peerCloneAndroidSource'
import type { PeerCloneInvoke } from './peerClone'

const prepared = {
    sessionId: '11111111-1111-4111-8111-111111111111',
    manifestId: 'a'.repeat(64),
    phase: 'prepared' as const,
    devices: [],
}

describe('Android P1 source facade', () => {
    it('starts only after an explicit service request and forwards identity only', async () => {
        const calls: Array<[string, unknown]> = []
        const foreground = { lane: 'p1-source' as const, operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
        const facade = createAndroidPeerCloneSourceFacade({
            invoke: vi.fn(async (command, args) => {
                calls.push([command, args])
                if (command.endsWith('_reserve')) return foreground
                if (command.endsWith('_start')) return { ...prepared, phase: 'running', pairingUri: 'risuailocal://peer-clone/v1' }
                return prepared
            }) as PeerCloneInvoke,
            bridge: { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) },
            flushPendingData: vi.fn(async () => undefined),
        })

        await facade.start(prepared.sessionId)

        expect(facade.foregroundIdentity()).toEqual(foreground)
        expect(calls.map(([command]) => command)).toEqual([
            'peer_clone_android_source_reserve',
            'peer_clone_android_source_start',
        ])
        expect(JSON.stringify(calls)).not.toMatch(/claim|bearer|token|pairing/i)
    })

    it('can restart the retained prepared source after notification Stop', async () => {
        let generation = 0
        const bridge = { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) }
        const invoke = vi.fn(async (command: string) => {
            if (command.endsWith('_reserve')) {
                generation += 1
                return { lane: 'p1-source', operationId: `${generation}1111111-1111-4111-8111-111111111111`, generation }
            }
            if (command.endsWith('_status')) return prepared
            return { ...prepared, phase: 'running' }
        })
        const facade = createAndroidPeerCloneSourceFacade({
            invoke: invoke as PeerCloneInvoke,
            bridge,
            flushPendingData: vi.fn(async () => undefined),
        })

        await facade.start(prepared.sessionId)
        await facade.status()
        await facade.start(prepared.sessionId)

        expect(bridge.startSource).toHaveBeenCalledTimes(2)
        expect(generation).toBe(2)
    })

    it('reports LAN source capability with tunnels disabled', async () => {
        const facade = createAndroidPeerCloneSourceFacade({
            invoke: vi.fn(async () => ({ sourceReady: true, productionEnabled: true, tunnelReady: false })) as PeerCloneInvoke,
            bridge: { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) },
            flushPendingData: vi.fn(async () => undefined),
        })
        await expect(facade.capabilities()).resolves.toMatchObject({ tunnelReady: false })
        expect('startQuickTunnel' in facade).toBe(false)
        expect('startNamedTunnel' in facade).toBe(false)
    })
})
