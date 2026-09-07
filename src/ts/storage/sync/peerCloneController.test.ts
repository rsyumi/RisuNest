import { afterEach, describe, expect, it, vi } from 'vitest'

import {
    createPeerCloneFacade,
    initialPeerCloneState,
    type PeerCloneInvoke,
    type PeerCloneReplacementRuntime,
} from './peerClone'
import { createPeerCloneController } from './peerCloneController'

const claimedTarget = {
    endpoint: 'http://192.168.1.4:43123/',
    sessionId: '123e4567-e89b-12d3-a456-426614174000',
    manifestId: 'a'.repeat(64),
}

afterEach(() => {
    vi.useRealTimers()
})

describe('peer clone controller lifecycle', () => {
    it('ignores a progress response delivered after cancellation succeeds', async () => {
        vi.useFakeTimers()
        let resolveStatus!: (status: unknown) => void
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_target_status') {
                return await new Promise((resolve) => { resolveStatus = resolve }) as T
            }
            return undefined as T
        })
        const facade = createPeerCloneFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerCloneInvoke,
        })
        const controller = createPeerCloneController({ facade, targetPollMilliseconds: 10 })
        controller.joinClaimed(claimedTarget)
        controller.confirmDestructiveReplace()
        await controller.download()
        await vi.advanceTimersByTimeAsync(10)
        await controller.cancel()

        resolveStatus({ phase: 'downloading', completedBytes: 8, totalBytes: 10 })
        await vi.advanceTimersByTimeAsync(0)

        expect(facade.getState().target.phase).toBe('cancelled')
        expect(controller.snapshot().state.target.phase).toBe('cancelled')
        expect(controller.snapshot().targetPhase).toBe('cancelled')
        expect(controller.snapshot().error).toBe('')
    })

    it('initializes only target capabilities for the unified controller', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            return undefined as T
        })
        const controller = createPeerCloneController({
            facade: createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: runtime(vi.fn(async () => undefined), vi.fn()),
            }),
        })

        await controller.initialize()

        expect(controller.snapshot().capabilities).toMatchObject({ productionEnabled: true })
        // Initialization reads capabilities and nothing else: no target join and
        // no progress poll happen before the page asks for one.
        expect(invoke.mock.calls.map(([command]) => command)).toEqual(['peer_clone_capabilities'])
    })

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
        controller.joinClaimed(claimedTarget)
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

    it('retries initialization after a transient failure instead of caching it', async () => {
        let failCapabilities = true
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') {
                if (failCapabilities) {
                    failCapabilities = false
                    throw new Error('capabilities unavailable')
                }
                return capabilities() as T
            }
            return undefined as T
        })
        const controller = createPeerCloneController({
            facade: createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: runtime(vi.fn(async (_revision: number) => undefined), vi.fn()),
            }),
        })

        await controller.initialize()
        expect(controller.snapshot().error).toBe('capabilities unavailable')
        expect(controller.snapshot().capabilities).toBeUndefined()

        await controller.initialize()

        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_capabilities')).toHaveLength(2)
        expect(controller.snapshot().error).toBe('')
        expect(controller.snapshot().capabilities).toMatchObject({ productionEnabled: true })
    })

    it('preserves a native target failure until resume is accepted', async () => {
        vi.useFakeTimers()
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_target_status') {
                return {
                    phase: 'failed',
                    completedBytes: 64,
                    totalBytes: 128,
                    error: 'clone object hash mismatch',
                } as T
            }
            return undefined as T
        })
        const controller = createPeerCloneController({
            facade: createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: runtime(vi.fn(async (_revision: number) => undefined), vi.fn()),
            }),
            targetPollMilliseconds: 10,
        })
        controller.joinClaimed(claimedTarget)
        controller.confirmDestructiveReplace()
        await controller.download()
        await vi.advanceTimersByTimeAsync(10)

        expect(controller.snapshot().state.target.phase).toBe('failed')
        expect(controller.snapshot().error).toBe('clone object hash mismatch')

        await controller.resume()
        expect(controller.snapshot().error).toBe('')
    })
})

describe('peer clone controller surface', () => {
    it('exposes only the target side', () => {
        const controller = createPeerCloneController({
            facade: createPeerCloneFacade({ platform: 'desktop', invoke: (async () => ({})) as never }),
        })
        expect(Object.keys(controller).sort()).toEqual([
            'cancel', 'confirmDestructiveReplace', 'download', 'initialize', 'joinClaimed',
            'resume', 'snapshot', 'subscribe',
        ])
        expect(controller.snapshot()).toEqual({ state: initialPeerCloneState, error: '', warning: '' })
    })
})

function capabilities() {
    return {
        desktop: true,
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
