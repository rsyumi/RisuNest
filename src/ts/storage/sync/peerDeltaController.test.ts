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
})
