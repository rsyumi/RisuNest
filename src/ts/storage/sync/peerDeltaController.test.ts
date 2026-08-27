import { describe, expect, test, vi } from 'vitest'

import { createPeerDeltaController } from './peerDeltaController'
import type { createPeerDeltaFacade } from './peerDelta'

type Facade = ReturnType<typeof createPeerDeltaFacade>

function facadeFixture(overrides: Partial<Facade> = {}): Facade {
    return {
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
        status: vi.fn(async () => ({ phase: 'idle', devices: [] })),
        stop: vi.fn(async () => undefined),
        revoke: vi.fn(async () => undefined),
        pull: vi.fn(async () => ({
            kind: 'noChanges',
            revision: 1,
            transferredObjects: 0,
            transferredBytes: 0,
        })),
        ...overrides,
    } as Facade
}

describe('peer delta controller', () => {
    test('keeps source ownership and one in-flight pull outside component subscriptions', async () => {
        let resolvePull!: (value: Awaited<ReturnType<Facade['pull']>>) => void
        const pull = vi.fn(() => new Promise<Awaited<ReturnType<Facade['pull']>>>((resolve) => {
            resolvePull = resolve
        }))
        const facade = facadeFixture({ pull })
        const controller = createPeerDeltaController({ facade, sourcePollMilliseconds: 60_000 })
        const unsubscribe = controller.subscribe(() => undefined)
        await controller.initialize()
        await controller.prepare()
        await controller.start('session')

        const first = controller.pull('pairing')
        const second = controller.pull('pairing')
        unsubscribe()
        expect(second).toBe(first)
        expect(pull).toHaveBeenCalledOnce()
        expect(controller.snapshot().pullPhase).toBe('running')

        resolvePull({
            kind: 'updated',
            revision: 2,
            transferredObjects: 1,
            transferredBytes: 20,
        })
        await expect(first).resolves.toMatchObject({ kind: 'updated' })
        await expect(second).resolves.toMatchObject({ kind: 'updated' })
        expect(controller.snapshot()).toMatchObject({
            pullPhase: 'completed',
            pullResult: { kind: 'updated', revision: 2 },
            sourceStatus: { phase: 'running', sessionId: 'session' },
        })
    })

    test('projects full-clone-required without converting it to a generic failure', async () => {
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                pull: vi.fn(async () => ({
                    kind: 'fullCloneRequired',
                    reason: 'noExactCommonBase',
                } as const)),
            }),
        })
        await controller.initialize()

        await expect(controller.pull('pairing')).resolves.toEqual({
            kind: 'fullCloneRequired',
            reason: 'noExactCommonBase',
        })
        expect(controller.snapshot().pullPhase).toBe('fullCloneRequired')
        expect(controller.snapshot().error).toBe('')
    })

    test('rejects a different pairing while a pull remains active across resubscription', async () => {
        let resolvePull!: (value: Awaited<ReturnType<Facade['pull']>>) => void
        const controller = createPeerDeltaController({
            facade: facadeFixture({
                pull: vi.fn(() => new Promise<Awaited<ReturnType<Facade['pull']>>>((resolve) => {
                    resolvePull = resolve
                })),
            }),
        })
        const unsubscribe = controller.subscribe(() => undefined)
        const active = controller.pull('first-pairing')
        unsubscribe()
        const resubscribe = controller.subscribe(() => undefined)

        await expect(controller.pull('second-pairing')).rejects.toThrow('different pairing')
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
                pull: vi.fn(async () => { throw new Error('renderer refresh failed') }),
            }),
            sourcePollMilliseconds: 10,
        })
        await controller.start('session')

        await expect(controller.pull('pairing')).rejects.toThrow('renderer refresh failed')
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
})
