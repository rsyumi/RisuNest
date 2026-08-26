import { afterEach, describe, expect, it, vi } from 'vitest'

import {
    createPeerCloneFacade,
    type PeerCloneInvoke,
    type PeerCloneReplacementRuntime,
} from './peerClone'
import { createPeerCloneController } from './peerCloneController'

const claim = 'b'.repeat(64)
const pairingUri = `risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.4%3A43123&session=123e4567-e89b-12d3-a456-426614174000&manifest=${'a'.repeat(64)}#claim=${claim}`

afterEach(() => {
    vi.useRealTimers()
})

describe('peer clone controller lifecycle', () => {
    it('keeps retrying a committed renderer refresh after every view unsubscribes', async () => {
        vi.useFakeTimers()
        let finalized = false
        let refreshAttempt = 0
        const release = vi.fn()
        const refresh = vi.fn(async (_revision: number) => {
            if (refreshAttempt++ === 0) throw new Error('refresh failed')
        })
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_target_status') {
                return {
                    phase: finalized ? 'completed' : 'awaitingActivation',
                    completedBytes: 10,
                    totalBytes: 10,
                } as T
            }
            if (command === 'peer_clone_finalize') {
                finalized = true
                return { revision: 2 } as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
            runtime: runtime(refresh, release),
        })
        const controller = createPeerCloneController({ facade, targetPollMilliseconds: 10 })
        const unsubscribe = controller.subscribe(vi.fn())
        controller.join(pairingUri)
        controller.confirmDestructiveReplace()
        await controller.download()
        unsubscribe()

        await vi.advanceTimersByTimeAsync(10)
        expect(controller.snapshot().error).toBe('refresh failed')
        expect(release).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(10)

        expect(controller.snapshot().state.target.phase).toBe('completed')
        expect(controller.snapshot().error).toBe('')
        expect(refresh).toHaveBeenCalledTimes(2)
        expect(release).toHaveBeenCalledTimes(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_finalize')).toHaveLength(1)
    })

    it('retains source ownership without subscribers and makes stopping cleanup retryable', async () => {
        vi.useFakeTimers()
        let phase: 'running' | 'stopping' | 'stopped' = 'running'
        let stopAttempt = 0
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_status') {
                return { phase, sessionId: 'source-session', devices: [] } as T
            }
            if (command === 'peer_clone_stop') {
                if (stopAttempt++ === 0) {
                    phase = 'stopping'
                    throw new Error('cleanup failed')
                }
                phase = 'stopped'
            }
            return undefined as T
        })
        const controller = createPeerCloneController({
            facade: createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: runtime(vi.fn(async (_revision: number) => undefined), vi.fn()),
            }),
            sourcePollMilliseconds: 10,
        })
        const unsubscribe = controller.subscribe(vi.fn())
        await controller.initialize()
        unsubscribe()

        await expect(controller.stop('source-session')).rejects.toThrow('cleanup failed')
        expect(controller.snapshot().sourceStatus.phase).toBe('stopping')
        await vi.advanceTimersByTimeAsync(10)
        await expect(controller.stop('source-session')).resolves.toBeUndefined()

        expect(controller.snapshot().sourceStatus.phase).toBe('stopped')
        expect(controller.snapshot().error).toBe('')
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_stop')).toHaveLength(2)
    })
})

function capabilities() {
    return {
        desktop: true,
        sourceReady: true,
        atomicActivationReady: true,
        losslessBackupReady: true,
        httpTransportReady: true,
        largeFixturePassed: false,
        productionEnabled: true,
    }
}

function runtime(
    refresh: (revision: number) => Promise<void>,
    release: () => void,
): PeerCloneReplacementRuntime {
    return {
        flushPendingData: vi.fn(async () => undefined),
        capturePersistentMutationToken: vi.fn(async () => ({ revision: 1, mutationGeneration: 0 })),
        acquireDestructiveReplacementFence: vi.fn(async () => ({
            refreshCommittedWorkingSet: refresh,
            release,
        })),
    }
}
