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
    it.each([
        ['false', false],
        ['throw', true],
        ['timeout', true],
    ] as const)(
        'abandons the exact native reservation after %s start failure only with definitive start failure or accepted Stop',
        async (failure, expectsStop) => {
            const foreground = { lane: 'p1-source' as const, operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
            let owner: typeof foreground | undefined
            const bridge = {
                startSource: vi.fn(() => {
                    if (failure === 'throw') throw new Error('service throw')
                    return failure !== 'false'
                }),
                stopSource: vi.fn(() => true),
            }
            const invoke = vi.fn(async (command: string) => {
                if (command.endsWith('_reserve')) {
                    if (owner) throw new Error('owner retained')
                    owner = foreground
                    return foreground
                }
                if (command === 'peer_sync_foreground_source_abandon') {
                    owner = undefined
                    return true
                }
                if (command.endsWith('_start') && failure === 'timeout') throw new Error('attach timeout')
                return { ...prepared, phase: 'running' }
            })
            const facade = createAndroidPeerCloneSourceFacade({
                invoke: invoke as PeerCloneInvoke,
                bridge,
                flushPendingData: vi.fn(async () => undefined),
            })

            await expect(facade.start(prepared.sessionId)).rejects.toThrow()
            if (expectsStop) {
                expect(bridge.stopSource).toHaveBeenCalledWith(
                    foreground.lane,
                    foreground.operationId,
                    foreground.generation,
                )
            } else {
                expect(bridge.stopSource).not.toHaveBeenCalled()
            }
            expect(invoke).toHaveBeenCalledWith('peer_sync_foreground_source_abandon', { foreground })
            expect(owner).toBeUndefined()
        },
    )

    it.each(['false', 'throw'] as const)(
        'retains uncertain native ownership when exact Stop returns %s and recovers before a fresh reserve',
        async (stopFailure) => {
            const foreground = { lane: 'p1-source' as const, operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
            let owner: typeof foreground | undefined
            let stopAttempt = 0
            let startThrows = true
            const bridge = {
                startSource: vi.fn(() => {
                    if (startThrows) throw new Error('uncertain START')
                    return true
                }),
                stopSource: vi.fn(() => {
                    stopAttempt += 1
                    if (stopAttempt === 1) {
                        if (stopFailure === 'throw') throw new Error('uncertain Stop')
                        return false
                    }
                    return true
                }),
            }
            const invoke = vi.fn(async (command: string) => {
                if (command === 'peer_sync_foreground_source_status') return owner ?? null
                if (command.endsWith('_reserve')) {
                    if (owner) throw new Error('owner retained')
                    owner = foreground
                    return foreground
                }
                if (command === 'peer_sync_foreground_source_abandon') {
                    owner = undefined
                    return true
                }
                return { ...prepared, phase: 'running' }
            })
            const facade = createAndroidPeerCloneSourceFacade({
                invoke: invoke as PeerCloneInvoke,
                bridge,
                flushPendingData: vi.fn(async () => undefined),
            })

            await expect(facade.start(prepared.sessionId)).rejects.toThrow()
            expect(owner).toEqual(foreground)
            expect(invoke).not.toHaveBeenCalledWith('peer_sync_foreground_source_abandon', { foreground })

            startThrows = false
            await facade.start(prepared.sessionId)

            expect(stopAttempt).toBe(2)
            expect(owner).toEqual(foreground)
            expect(invoke).toHaveBeenCalledWith('peer_sync_foreground_source_abandon', { foreground })
        },
    )

    it('retains both the primary start error and exact-abandon failure', async () => {
        const foreground = { lane: 'p1-source' as const, operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
        const primary = new Error('attach timeout')
        const cleanup = new Error('abandon response lost')
        const facade = createAndroidPeerCloneSourceFacade({
            invoke: vi.fn(async (command) => {
                if (command === 'peer_sync_foreground_source_status') return null
                if (command.endsWith('_reserve')) return foreground
                if (command === 'peer_sync_foreground_source_abandon') throw cleanup
                throw primary
            }) as PeerCloneInvoke,
            bridge: { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) },
            flushPendingData: vi.fn(async () => undefined),
        })

        const failure = await facade.start(prepared.sessionId).catch((error: unknown) => error)

        expect(failure).toBeInstanceOf(AggregateError)
        expect((failure as AggregateError).errors).toEqual([primary, cleanup])
    })

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
            'peer_sync_foreground_source_status',
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

    it('fully stops a reconstructed running source through the native foreground identity', async () => {
        const foreground = { lane: 'p1-source' as const, operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
        const bridge = { startSource: vi.fn(() => true), stopSource: vi.fn(() => true) }
        const invoke = vi.fn(async (command: string) => {
            if (command.endsWith('_status')) return { ...prepared, phase: 'running' }
            if (command.endsWith('_stop')) return foreground
            return prepared
        })
        const facade = createAndroidPeerCloneSourceFacade({
            invoke: invoke as PeerCloneInvoke,
            bridge,
            flushPendingData: vi.fn(async () => undefined),
        })

        await facade.status()
        await facade.stop(prepared.sessionId)

        expect(bridge.stopSource).toHaveBeenCalledTimes(1)
        expect(bridge.stopSource).toHaveBeenCalledWith(
            foreground.lane,
            foreground.operationId,
            foreground.generation,
        )
    })

    it('fails closed when the foreground service rejects an exact source stop', async () => {
        const foreground = { lane: 'p1-source' as const, operationId: '22222222-2222-4222-8222-222222222222', generation: 4 }
        const bridge = { startSource: vi.fn(() => true), stopSource: vi.fn(() => false) }
        const invoke = vi.fn(async (command: string) => {
            if (command.endsWith('_stop')) return foreground
            return prepared
        })
        const facade = createAndroidPeerCloneSourceFacade({
            invoke: invoke as PeerCloneInvoke,
            bridge,
            flushPendingData: vi.fn(async () => undefined),
        })

        await expect(facade.stop(prepared.sessionId)).rejects.toThrow(
            'Android peer clone foreground service could not stop',
        )
        expect(bridge.stopSource).toHaveBeenCalledWith(
            foreground.lane,
            foreground.operationId,
            foreground.generation,
        )
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
