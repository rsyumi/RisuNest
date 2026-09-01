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
    it('initializes only target capabilities for the unified controller', async () => {
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_status') return { phase: 'running', devices: [] } as T
            return undefined as T
        })
        const controller = createPeerCloneController({
            facade: createPeerCloneFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerCloneInvoke,
                runtime: runtime(vi.fn(async () => undefined), vi.fn()),
            }),
        })

        await controller.initializeTarget()

        expect(controller.snapshot().capabilities).toMatchObject({ productionEnabled: true })
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_status')).toBe(false)
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
            if (command === 'peer_clone_status') {
                return { phase: 'idle', devices: [] } as T
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
        controller.join(pairingUri)
        controller.confirmDestructiveReplace()
        await controller.download()
        await vi.advanceTimersByTimeAsync(10)

        expect(controller.snapshot().state.target.phase).toBe('failed')
        expect(controller.snapshot().error).toBe('clone object hash mismatch')

        await controller.resume()
        expect(controller.snapshot().error).toBe('')
    })

    it('clears the one-shot pairing URI when host shutdown succeeds before cleanup fails', async () => {
        let phase: 'prepared' | 'running' | 'stopping' = 'prepared'
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_start') {
                phase = 'running'
                return {
                    phase,
                    sessionId: 'source-session',
                    pairingUri,
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_stop') {
                phase = 'stopping'
                throw new Error('cleanup failed')
            }
            if (command === 'peer_clone_status') {
                return { phase, sessionId: 'source-session', devices: [] } as T
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

        await controller.start('source-session')
        expect(controller.snapshot().sourcePairingUri).toBe(pairingUri)
        await expect(controller.stop('source-session')).rejects.toThrow('cleanup failed')

        expect(controller.snapshot().sourceStatus.phase).toBe('stopping')
        expect(controller.snapshot().sourcePairingUri).toBe('')
    })

    it('owns Quick Tunnel polling and retries tunnel cleanup through the shared source lifecycle', async () => {
        vi.useFakeTimers()
        let phase: 'running' | 'stopping' | 'stopped' = 'running'
        let stopAttempt = 0
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_tunnel_start') {
                return {
                    phase: 'running',
                    sessionId: 'source-session',
                    pairingUri,
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_status') {
                return {
                    phase,
                    sessionId: 'source-session',
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_tunnel_status') {
                return {
                    phase,
                    sessionId: 'source-session',
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                } as T
            }
            if (command === 'peer_clone_tunnel_stop') {
                if (stopAttempt++ === 0) {
                    phase = 'stopping'
                    throw new Error('tunnel cleanup failed')
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

        await controller.startQuickTunnel('source-session')
        expect(controller.snapshot().sourcePairingUri).toBe(pairingUri)
        expect(controller.snapshot().tunnelStatus).toMatchObject({
            phase: 'running',
            tunnel: { kind: 'quick' },
        })

        await expect(controller.stop('source-session')).rejects.toThrow('tunnel cleanup failed')
        expect(controller.snapshot().sourceStatus.phase).toBe('stopping')
        expect(controller.snapshot().sourcePairingUri).toBe('')
        await vi.advanceTimersByTimeAsync(10)
        await expect(controller.stop('source-session')).resolves.toBeUndefined()

        expect(controller.snapshot().sourceStatus.phase).toBe('stopped')
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_clone_tunnel_stop')).toHaveLength(2)
        expect(invoke.mock.calls.some(([command]) => command === 'peer_clone_stop')).toBe(false)
    })

    it('clears the one-shot pairing link when a tunnel exits naturally', async () => {
        vi.useFakeTimers()
        let phase: 'running' | 'stopped' = 'running'
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_tunnel_start') {
                return {
                    phase: 'running',
                    sessionId: 'source-session',
                    pairingUri,
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_status') {
                return {
                    phase,
                    sessionId: 'source-session',
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_tunnel_status') {
                return {
                    phase,
                    sessionId: 'source-session',
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
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
            sourcePollMilliseconds: 10,
        })

        await controller.startQuickTunnel('source-session')
        expect(controller.snapshot().sourcePairingUri).toBe(pairingUri)

        phase = 'stopped'
        await vi.advanceTimersByTimeAsync(10)

        expect(controller.snapshot().sourceStatus.phase).toBe('stopped')
        expect(controller.snapshot().sourcePairingUri).toBe('')
    })

    it('never retains a Named Tunnel token or reflected native failure in controller state', async () => {
        const token = 'named-tunnel-token-that-is-at-least-32-bytes'
        const reflected = `${token} https://sync.example.com/v1/sessions/id/tunnel-check/probe-secret`
        const invoke = vi.fn<PeerCloneInvoke>(async <T>(command: string): Promise<T> => {
            if (command === 'peer_clone_capabilities') return capabilities() as T
            if (command === 'peer_clone_tunnel_start') throw new Error(reflected)
            if (command === 'peer_clone_status') {
                return {
                    phase: 'stopping',
                    sessionId: 'source-session',
                    tunnel: { kind: 'named', experimental: false, oneShot: false },
                    devices: [],
                } as T
            }
            if (command === 'peer_clone_tunnel_status') {
                return {
                    phase: 'stopping',
                    sessionId: 'source-session',
                    tunnel: { kind: 'named', experimental: false, oneShot: false },
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
        })

        await expect(
            controller.startNamedTunnel('source-session', token, 'https://sync.example.com'),
        ).rejects.toThrow('Named tunnel failed to start')

        expect(controller.snapshot().error).toBe('Named tunnel failed to start')
        expect(controller.snapshot().sourceStatus).toMatchObject({
            phase: 'stopping',
            tunnel: { kind: 'named' },
        })
        expect(controller.snapshot().tunnelStatus.phase).toBe('stopping')
        expect(JSON.stringify(controller.snapshot())).not.toContain(token)
        expect(JSON.stringify(controller.snapshot())).not.toContain('tunnel-check')
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
