import { describe, expect, it, vi } from 'vitest'

import { createPeerAndroidSourceForeground } from './peerAndroidSourceForeground'
import type { PeerSyncInvoke } from './peerSyncShared'

const foreground = {
    lane: 'p4-source' as const,
    operationId: '44444444-4444-4444-8444-444444444444',
    generation: 9,
}

function fixture(startSource: () => boolean) {
    const calls: string[] = []
    const invoke = vi.fn(async <T>(command: string): Promise<T> => {
        calls.push(command)
        if (command === 'peer_sync_foreground_source_status') return null as T
        if (command === 'lane_reserve') return foreground as T
        if (command === 'lane_start') return { phase: 'running' } as T
        if (command === 'peer_sync_foreground_source_abandon') return true as T
        throw new Error('Unexpected command: ' + command)
    })
    const bridge = { startSource: vi.fn(startSource), stopSource: vi.fn(() => true) }
    const lifecycle = createPeerAndroidSourceForeground({
        invoke: invoke as PeerSyncInvoke,
        bridge,
        lane: 'p4-source',
        reserveCommand: 'lane_reserve',
        startCommand: 'lane_start',
        startArgs: (sessionId, identity) => ({ sessionId, foreground: identity }),
        staleIdentityError: 'stale identity',
        serviceStartError: 'could not start',
        serviceStopError: 'could not stop',
        cleanupFailureMessage: 'start and cleanup both failed',
    })
    return { calls, invoke, bridge, lifecycle }
}

describe('Android peer source foreground lifecycle', () => {
    it('abandons without service Stop when service start returns false', async () => {
        const { calls, bridge, lifecycle } = fixture(() => false)

        await expect(lifecycle.start('session')).rejects.toThrow('could not start')

        expect(calls).toEqual([
            'peer_sync_foreground_source_status',
            'lane_reserve',
            'peer_sync_foreground_source_abandon',
        ])
        expect(bridge.stopSource).not.toHaveBeenCalled()
    })

    it('stops then abandons after native start failure and preserves cleanup errors', async () => {
        const { calls, invoke, bridge, lifecycle } = fixture(() => true)
        const primary = new Error('native start failed')
        const cleanup = new Error('abandon response lost')
        invoke.mockImplementation(async <T>(command: string): Promise<T> => {
            calls.push(command)
            if (command === 'peer_sync_foreground_source_status') return null as T
            if (command === 'lane_reserve') return foreground as T
            if (command === 'lane_start') throw primary
            if (command === 'peer_sync_foreground_source_abandon') throw cleanup
            throw new Error('Unexpected command: ' + command)
        })

        const error = await lifecycle.start('session').catch((cause: unknown) => cause)

        expect(bridge.stopSource).toHaveBeenCalledWith('p4-source', foreground.operationId, 9)
        expect(calls).toEqual([
            'peer_sync_foreground_source_status',
            'lane_reserve',
            'lane_start',
            'peer_sync_foreground_source_abandon',
        ])
        expect(error).toBeInstanceOf(AggregateError)
        expect((error as AggregateError).errors).toEqual([primary, cleanup])
    })

    it('recovers an exact stale source reservation before reserving a fresh start', async () => {
        const { calls, invoke, bridge, lifecycle } = fixture(() => true)
        invoke.mockImplementation(async <T>(command: string): Promise<T> => {
            calls.push(command)
            if (command === 'peer_sync_foreground_source_status') return foreground as T
            if (command === 'peer_sync_foreground_source_abandon') return true as T
            if (command === 'lane_reserve') return foreground as T
            if (command === 'lane_start') return { phase: 'running' } as T
            throw new Error('Unexpected command: ' + command)
        })

        await expect(lifecycle.start('session')).resolves.toMatchObject({ foreground })

        expect(bridge.stopSource).toHaveBeenCalledWith('p4-source', foreground.operationId, 9)
        expect(calls).toEqual([
            'peer_sync_foreground_source_status',
            'peer_sync_foreground_source_abandon',
            'lane_reserve',
            'lane_start',
        ])
    })
})
