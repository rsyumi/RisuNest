import { describe, expect, test, vi } from 'vitest'

import { createPeerDeltaController, getAndroidPeerDeltaController } from './peerDeltaController'
import type { createPeerDeltaFacade, PeerDeltaMutationRuntime } from './peerDelta'

type Facade = ReturnType<typeof createPeerDeltaFacade>

function facadeFixture(overrides: Partial<Facade> = {}): Facade {
    return {
        recoverTargetForeground: vi.fn(async () => undefined),
        capabilities: vi.fn(async () => ({
            desktop: true,
            sourceReady: true,
            atomicActivationReady: true,
            authenticatedTransportReady: true,
            productionEnabled: true,
        })),
        prepare: vi.fn(async () => ({
            phase: 'prepared',
            sessionId: 'session',
            manifestId: 'a'.repeat(64),
            devices: [],
        })),
        start: vi.fn(async () => ({
            phase: 'running',
            sessionId: 'session',
            manifestId: 'a'.repeat(64),
            pairingUri: 'risuailocal://peer-delta/v1',
            devices: [],
        })),
        startQuickTunnel: vi.fn(async () => ({
            phase: 'running', sessionId: 'session', manifestId: 'a'.repeat(64),
            pairingUri: 'risuailocal://peer-delta/v1',
            tunnel: { kind: 'quick', experimental: true, oneShot: true }, devices: [],
        })),
        startNamedTunnel: vi.fn(async () => ({
            phase: 'running', sessionId: 'session', manifestId: 'a'.repeat(64),
            pairingUri: 'risuailocal://peer-delta/v1',
            tunnel: { kind: 'named', experimental: false, oneShot: false }, devices: [],
        })),
        tunnelStatus: vi.fn(async () => ({ phase: 'idle' })),
        stopTunnel: vi.fn(async () => undefined),
        status: vi.fn(async () => ({ phase: 'idle', devices: [] })),
        retained: vi.fn(async () => null),
        abandonRetained: vi.fn(async () => undefined),
        stop: vi.fn(async () => undefined),
        revoke: vi.fn(async () => undefined),
        pullRegistered: vi.fn(async () => ({
            kind: 'noChanges',
            revision: 1,
            transferredObjects: 0,
            transferredBytes: 0,
        })),
        ...overrides,
    } as Facade
}

describe('peer delta controller', () => {
    test('initializes target recovery without reading or polling legacy source status', async () => {
        const targetFacade = facadeFixture()
        const controller = createPeerDeltaController({ facade: targetFacade })

        await controller.initializeTarget()

        expect(targetFacade.recoverTargetForeground).toHaveBeenCalledTimes(1)
        expect(targetFacade.capabilities).toHaveBeenCalledTimes(1)
        expect(targetFacade.status).not.toHaveBeenCalled()
    })

    test('caches one module-level Android controller so retained pull fences survive settings remounts', () => {
        const runtime = (): PeerDeltaMutationRuntime => ({
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 1, mutationGeneration: 0 })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(async () => undefined),
                release: vi.fn(),
            })),
        })

        const first = getAndroidPeerDeltaController(runtime())
        const second = getAndroidPeerDeltaController(runtime())

        expect(second).toBe(first)
    })

    test('recovers native Android target foreground ownership before reconstructed status', async () => {
        const events: string[] = []
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                recoverTargetForeground: vi.fn(async () => { events.push('recover-target') }),
                capabilities: vi.fn(async () => {
                    events.push('capabilities')
                    return {
                        desktop: false,
                        sourceReady: true,
                        atomicActivationReady: true,
                        authenticatedTransportReady: true,
                        productionEnabled: true,
                        tunnelReady: false,
                    }
                }),
                status: vi.fn(async () => {
                    events.push('source-status')
                    return { phase: 'stopped', devices: [] } as const
                }),
            }),
        })

        await controller.initialize()

        expect(events[0]).toBe('recover-target')
        expect(events.slice(1).sort()).toEqual(['capabilities', 'source-status'])
        expect(controller.snapshot().sourceStatus.phase).toBe('stopped')
    })

    test('refreshes exact native cleanup ownership after an uncertain tunnel start failure', async () => {
        const stopping = {
            phase: 'stopping', sessionId: 'session', manifestId: 'a'.repeat(64),
            tunnel: { kind: 'quick', experimental: true, oneShot: true }, devices: [],
        } as const
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                startQuickTunnel: vi.fn(async () => { throw new Error('tunnel failed to start') }),
                status: vi.fn(async () => stopping),
                tunnelStatus: vi.fn(async () => ({
                    phase: 'stopping', sessionId: 'session', tunnel: stopping.tunnel,
                } as const)),
            }),
            sourcePollMilliseconds: 60_000,
        })

        await expect(controller.startQuickTunnel('session')).rejects.toThrow('tunnel failed to start')
        expect(controller.snapshot()).toMatchObject({
            sourceStatus: stopping,
            tunnelStatus: { phase: 'stopping', sessionId: 'session' },
            sourcePairingUri: '',
        })
    })

    test('owns tunnel polling, natural-exit cleanup, and exact stop retries', async () => {
        vi.useFakeTimers()
        const stopTunnel = vi.fn()
            .mockRejectedValueOnce(new Error('tunnel cleanup failed'))
            .mockResolvedValueOnce(undefined)
        const status = vi.fn(async () => stopTunnel.mock.calls.length < 2
            ? ({
                phase: 'stopping', sessionId: 'session', manifestId: 'a'.repeat(64),
                tunnel: { kind: 'quick', experimental: true, oneShot: true }, devices: [],
            } as const)
            : ({ phase: 'stopped', devices: [] } as const))
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                status,
                tunnelStatus: vi.fn(async () => ({
                    phase: 'stopped', sessionId: 'session',
                    tunnel: { kind: 'quick', experimental: true, oneShot: true },
                } as const)),
                stopTunnel,
            }),
            sourcePollMilliseconds: 10,
        })

        await controller.startQuickTunnel('session')
        await vi.advanceTimersByTimeAsync(10)
        await expect(controller.stop('session')).rejects.toThrow('tunnel cleanup failed')
        expect(controller.snapshot().sourceStatus.phase).toBe('stopping')
        await controller.stop('session')

        expect(stopTunnel).toHaveBeenCalledTimes(2)
        expect(controller.snapshot().sourcePairingUri).toBe('')
        vi.useRealTimers()
    })
    test('keeps source ownership and one in-flight pull outside component subscriptions', async () => {
        let resolvePull!: (value: Awaited<ReturnType<Facade['pullRegistered']>>) => void
        const pullRegistered = vi.fn(() => new Promise<Awaited<ReturnType<Facade['pullRegistered']>>>((resolve) => {
            resolvePull = resolve
        }))
        const facade = facadeFixture({ pullRegistered })
        const controller = createPeerDeltaController({ facade, sourcePollMilliseconds: 60_000 })
        const unsubscribe = controller.subscribe(() => undefined)
        await controller.initialize()
        await controller.prepare()
        await controller.start('session')

        const first = controller.pullRegistered('source-device')
        await expect(controller.pullRegistered('source-device'))
            .rejects.toThrow('A peer delta pull is already running')
        unsubscribe()
        expect(pullRegistered).toHaveBeenCalledOnce()
        expect(controller.snapshot().pullPhase).toBe('running')

        resolvePull({
            kind: 'updated',
            revision: 2,
            transferredObjects: 1,
            transferredBytes: 20,
        })
        await expect(first).resolves.toMatchObject({ kind: 'updated' })
        expect(controller.snapshot()).toMatchObject({
            pullPhase: 'completed',
            pullResult: { kind: 'updated', revision: 2 },
            sourceStatus: { phase: 'running', sessionId: 'session' },
        })
    })

    test('projects full-clone-required without converting it to a generic failure', async () => {
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                pullRegistered: vi.fn(async () => ({
                    kind: 'fullCloneRequired',
                    reason: 'noExactCommonBase',
                } as const)),
            }),
        })
        await controller.initialize()

        await expect(controller.pullRegistered('source-device')).resolves.toEqual({
            kind: 'fullCloneRequired',
            reason: 'noExactCommonBase',
        })
        expect(controller.snapshot().pullPhase).toBe('fullCloneRequired')
        expect(controller.snapshot().error).toBe('')
    })

    test('rejects a second registered pull while one remains active across resubscription', async () => {
        let resolvePull!: (value: Awaited<ReturnType<Facade['pullRegistered']>>) => void
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                pullRegistered: vi.fn(() => new Promise<Awaited<ReturnType<Facade['pullRegistered']>>>((resolve) => {
                    resolvePull = resolve
                })),
            }),
        })
        const unsubscribe = controller.subscribe(() => undefined)
        const active = controller.pullRegistered('first-device')
        unsubscribe()
        const resubscribe = controller.subscribe(() => undefined)

        await expect(controller.pullRegistered('second-device'))
            .rejects.toThrow('A peer delta pull is already running')
        expect(controller.snapshot().pullPhase).toBe('running')

        resolvePull({ kind: 'noChanges', revision: 1, transferredObjects: 0, transferredBytes: 0 })
        await expect(active).resolves.toMatchObject({ kind: 'noChanges' })
        resubscribe()
    })

    test('retains a failed refresh error after a successful source poll', async () => {
        vi.useFakeTimers()
        const sourceStatus = {
            phase: 'running',
            sessionId: 'session',
            manifestId: 'a'.repeat(64),
            pairingUri: 'risuailocal://peer-delta/v1',
            devices: [],
        } as const
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                start: vi.fn(async () => sourceStatus),
                status: vi.fn(async () => sourceStatus),
                pullRegistered: vi.fn(async () => { throw new Error('renderer refresh failed') }),
            }),
            sourcePollMilliseconds: 10,
        })
        await controller.start('session')

        await expect(controller.pullRegistered('source-device')).rejects.toThrow('renderer refresh failed')
        await vi.advanceTimersByTimeAsync(10)

        expect(controller.snapshot()).toMatchObject({
            sourceStatus,
            pullPhase: 'failed',
            error: 'renderer refresh failed',
        })
        vi.useRealTimers()
    })

    test('retries initialization after failure and recovers the running pairing URI', async () => {
        const sourceStatus = {
            phase: 'running',
            sessionId: 'session',
            manifestId: 'a'.repeat(64),
            pairingUri: 'risuailocal://peer-delta/v1?recovered=1',
            devices: [],
        } as const
        const capabilities = vi.fn()
            .mockRejectedValueOnce(new Error('capabilities unavailable'))
            .mockResolvedValueOnce({
                desktop: true,
                sourceReady: true,
                atomicActivationReady: true,
                authenticatedTransportReady: true,
                productionEnabled: true,
            })
        const controller = createPeerDeltaController({
            facade: facadeFixture({ capabilities, status: vi.fn(async () => sourceStatus) }),
            sourcePollMilliseconds: 60_000,
        })

        await controller.initialize()
        expect(controller.snapshot().error).toBe('capabilities unavailable')
        await controller.initialize()

        expect(capabilities).toHaveBeenCalledTimes(2)
        expect(controller.snapshot()).toMatchObject({
            sourceStatus,
            sourcePairingUri: sourceStatus.pairingUri,
            error: '',
        })
    })

    test('recovers the native-owned source pairing link during initialization', async () => {
        const sourceStatus = {
            phase: 'running',
            sessionId: 'session',
            manifestId: 'a'.repeat(64),
            pairingUri: 'risuailocal://peer-delta/v1?recovered=1',
            devices: [],
        } as const
        const controller = createPeerDeltaController({
            facade: facadeFixture({ status: vi.fn(async () => sourceStatus) }),
            sourcePollMilliseconds: 60_000,
        })

        await controller.initialize()

        expect(controller.snapshot()).toMatchObject({
            sourceStatus,
            sourcePairingUri: sourceStatus.pairingUri,
        })
    })
    test('reads the retained completion at target initialization and after every registered pull', async () => {
        const retained = {
            operationId: '00000000-0000-4000-8000-000000000091',
            sourceDeviceId: '00000000-0000-4000-8000-000000000093',
            sourceName: 'Desk',
            witness: 'uncommitted' as const,
            transferredObjects: 2,
            transferredBytes: 4096,
        }
        const readRetained = vi.fn(async () => retained)
        const facade = facadeFixture({
            retained: readRetained,
            pullRegistered: vi.fn(async () => { throw new Error('deltaCompletionRetained') }),
        })
        const controller = createPeerDeltaController({ facade })

        await controller.initializeTarget()
        expect(controller.snapshot().retained).toEqual(retained)
        expect(readRetained).toHaveBeenCalledTimes(1)

        await expect(controller.pullRegistered('source')).rejects.toThrow('deltaCompletionRetained')

        // A failed pull is exactly when the journal is most likely to be retained.
        expect(readRetained).toHaveBeenCalledTimes(2)
        expect(controller.snapshot().pullPhase).toBe('failed')
        expect(controller.snapshot().retained).toEqual(retained)
    })

    test('re-reads the retained completion after a registered pull succeeds', async () => {
        const readRetained = vi.fn(async () => null)
        const facade = facadeFixture({
            retained: readRetained,
            pullRegistered: vi.fn(async () => ({
                kind: 'updated' as const, revision: 3, transferredObjects: 1, transferredBytes: 8,
            })),
        })
        const controller = createPeerDeltaController({ facade })

        await controller.initializeTarget()
        await controller.pullRegistered('source')

        expect(readRetained).toHaveBeenCalledTimes(2)
        expect(controller.snapshot().retained).toBeNull()
        expect(controller.snapshot().pullPhase).toBe('completed')
    })

    test('keeps the last known retained state when the refresh after a pull cannot be read', async () => {
        const retained = {
            operationId: '00000000-0000-4000-8000-000000000091',
            sourceDeviceId: '00000000-0000-4000-8000-000000000093',
            sourceName: null,
            witness: 'ambiguous' as const,
            transferredObjects: 0,
            transferredBytes: 0,
        }
        const readRetained = vi.fn()
            .mockResolvedValueOnce(retained)
            .mockRejectedValueOnce(new Error('state unavailable'))
        const facade = facadeFixture({
            retained: readRetained,
            pullRegistered: vi.fn(async () => { throw new Error('deltaCompletionRetained') }),
        })
        const controller = createPeerDeltaController({ facade })

        await controller.initializeTarget()
        await expect(controller.pullRegistered('source')).rejects.toThrow('deltaCompletionRetained')

        expect(controller.snapshot().retained).toEqual(retained)
        expect(controller.snapshot().error).toBe('deltaCompletionRetained')
    })

    test('returns the target to idle with no error once the retained completion is abandoned', async () => {
        const retained = {
            operationId: '00000000-0000-4000-8000-000000000091',
            sourceDeviceId: '00000000-0000-4000-8000-000000000093',
            sourceName: 'Desk',
            witness: 'ambiguous' as const,
            transferredObjects: 0,
            transferredBytes: 0,
        }
        const readRetained = vi.fn()
            .mockResolvedValueOnce(retained)
            .mockResolvedValueOnce(retained)
            .mockResolvedValue(null)
        const abandonRetained = vi.fn(async () => undefined)
        const facade = facadeFixture({
            retained: readRetained,
            abandonRetained,
            pullRegistered: vi.fn(async () => { throw new Error('deltaCompletionRetained') }),
        })
        const controller = createPeerDeltaController({ facade })

        await controller.initializeTarget()
        await expect(controller.pullRegistered('source')).rejects.toThrow('deltaCompletionRetained')
        expect(controller.snapshot().pullPhase).toBe('failed')

        await controller.abandonRetained()

        expect(abandonRetained).toHaveBeenCalledWith(retained.operationId)
        expect(controller.snapshot().retained).toBeNull()
        expect(controller.snapshot().pullPhase).toBe('idle')
        expect(controller.snapshot().error).toBe('')
    })

    test('treats a re-read the abandonment outran as nothing left to report', async () => {
        const retained = {
            operationId: '00000000-0000-4000-8000-000000000091',
            sourceDeviceId: '00000000-0000-4000-8000-000000000093',
            sourceName: 'Desk',
            witness: 'ambiguous' as const,
            transferredObjects: 0,
            transferredBytes: 0,
        }
        const readRetained = vi.fn()
            .mockResolvedValueOnce(retained)
            .mockRejectedValue(new Error('state unavailable'))
        const controller = createPeerDeltaController({
            facade: facadeFixture({ retained: readRetained, abandonRetained: vi.fn(async () => undefined) }),
        })

        await controller.initializeTarget()
        await controller.abandonRetained()

        // The journal the retention named is already gone, so the target must
        // not be left holding the state it just dropped.
        expect(controller.snapshot().retained).toBeNull()
        expect(controller.snapshot().pullPhase).toBe('idle')
        expect(controller.snapshot().error).toBe('')
    })

    test('refuses a re-entrant registered pull while the retained refresh still runs', async () => {
        let releaseRefresh = (): void => {}
        const readRetained = vi.fn()
            .mockResolvedValueOnce(null)
            .mockImplementationOnce(() => new Promise((resolve) => { releaseRefresh = () => resolve(null) }))
        const pullRegistered = vi.fn(async () => ({
            kind: 'noChanges' as const, revision: 1, transferredObjects: 0, transferredBytes: 0,
        }))
        const controller = createPeerDeltaController({ facade: facadeFixture({ retained: readRetained, pullRegistered }) })

        await controller.initializeTarget()
        const pull = controller.pullRegistered('source')
        await vi.waitFor(() => expect(readRetained).toHaveBeenCalledTimes(2))

        // The fence outlives the refresh, so the second pull meets this
        // controller rather than the native target guard.
        await expect(controller.pullRegistered('source')).rejects.toThrow('A peer delta pull is already running')
        releaseRefresh()
        await pull

        expect(pullRegistered).toHaveBeenCalledTimes(1)
    })

    test('reports an abandonment failure on the target error channel', async () => {
        const retained = {
            operationId: '00000000-0000-4000-8000-000000000091',
            sourceDeviceId: '00000000-0000-4000-8000-000000000093',
            sourceName: 'Desk',
            witness: 'ambiguous' as const,
            transferredObjects: 0,
            transferredBytes: 0,
        }
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                retained: vi.fn(async () => retained),
                abandonRetained: vi.fn(async () => { throw new Error('operationFailed') }),
            }),
        })

        await controller.initializeTarget()
        await expect(controller.abandonRetained()).rejects.toThrow('operationFailed')

        expect(controller.snapshot().error).toBe('operationFailed')
        expect(controller.snapshot().retained).toEqual(retained)
    })
})
