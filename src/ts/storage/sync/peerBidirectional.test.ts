import { describe, expect, it, vi } from 'vitest'

import {
    createPeerBidirectionalFacade,
    parsePeerBidirectionalUri,
    PeerBidirectionalRefreshError,
    type PeerBidirectionalInvoke,
    type PeerBidirectionalMutationRuntime,
} from './peerBidirectional'

const pairingUri = 'risuailocal://peer-sync/v1?endpoint=http%3A%2F%2F192.168.1.20%3A32146'
    + '&session=123e4567-e89b-42d3-a456-426614174000'
    + `&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
const publicPairingUri = pairingUri.replace(
    'http%3A%2F%2F192.168.1.20%3A32146',
    'https%3A%2F%2Ffresh-public.example',
)

function runtime(log: string[]): PeerBidirectionalMutationRuntime {
    return {
        async flushPendingData(reason) {
            log.push(`flush:${reason}`)
        },
        async capturePersistentMutationToken(reason) {
            log.push(`capture:${reason}`)
            return { revision: 7, mutationGeneration: 11 }
        },
        async acquirePersistentMutationFence(token) {
            log.push(`acquire:${token.revision}:${token.mutationGeneration}`)
            return {
                async refreshCommittedWorkingSet(revision) {
                    log.push(`refresh:${revision}`)
                },
                release() {
                    log.push('release')
                },
            }
        },
    }
}

describe('peer bidirectional facade', () => {
    it('preserves a durable source-prepared operation in status', async () => {
        const status = {
            source: { phase: 'stopped' as const, devices: [] },
            operation: { phase: 'sourcePrepared' as const, operationId: 'operation-source-prepared' },
        }
        const nativeInvoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_status') return status
            throw new Error(`Unexpected command: ${command}`)
        })
        const peer = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: nativeInvoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(peer.status()).resolves.toEqual(status)
    })

    it('preserves a durable target-prepared operation in status', async () => {
        const status = {
            source: { phase: 'idle' as const, devices: [] },
            operation: { phase: 'targetPrepared' as const, operationId: 'operation-target-prepared' },
        }
        const nativeInvoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_status') return status
            throw new Error(`Unexpected command: ${command}`)
        })
        const peer = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: nativeInvoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(peer.status()).resolves.toEqual(status)
    })

    it('parses only the dedicated strict desktop pairing form', () => {
        expect(parsePeerBidirectionalUri(pairingUri)).toEqual({
            endpoint: 'http://192.168.1.20:32146',
            sessionId: '123e4567-e89b-42d3-a456-426614174000',
            manifestId: 'a'.repeat(64),
            claim: 'b'.repeat(64),
        })
        for (const invalid of [
            pairingUri.replace('peer-sync', 'peer-delta'),
            pairingUri.replace('/v1', '/v2'),
            pairingUri.replace('192.168.1.20', 'example.com'),
            pairingUri.replace(`#claim=${'b'.repeat(64)}`, ''),
            `${pairingUri}&extra=true`,
        ]) {
            expect(() => parsePeerBidirectionalUri(invalid)).toThrow('Invalid peer sync pairing URI')
        }
    })

    it.each([
        ['127.23.4.5', 'http://127.23.4.5:32146'],
        ['%5B%3A%3A1%5D', 'http://[::1]:32146'],
    ])('accepts the supported LAN loopback endpoint %s', (encodedHost, endpoint) => {
        const loopbackPairing = pairingUri.replace('192.168.1.20', encodedHost)
        expect(parsePeerBidirectionalUri(loopbackPairing).endpoint).toBe(endpoint)
    })

    it('does not create a fallback authority on web', async () => {
        const platform = 'web' as const
        const facade = createPeerBidirectionalFacade({ platform })
        await expect(facade.capabilities()).rejects.toThrow(`Peer sync is unsupported on ${platform}`)
        await expect(facade.sync(pairingUri)).rejects.toThrow(`Peer sync is unsupported on ${platform}`)
    })

    it('flushes and fences the source revision from prepare until stop completes', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${String(args?.expectedRevision ?? '')}`)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_start') {
                return { phase: 'running', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return { source: { phase: 'stopped', sessionId: 'session-source', devices: [] } }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await facade.prepare()
        await facade.start('session-source')
        expect(events).toEqual([
            'flush:peer-bidirectional-source-prepare',
            'capture:peer-bidirectional-source-prepare',
            'acquire:7:11',
            'invoke:peer_bidirectional_prepare:7',
            'invoke:peer_bidirectional_start:',
        ])

        await facade.stop('session-source')
        expect(events.slice(-3)).toEqual([
            'invoke:peer_bidirectional_stop:',
            'invoke:peer_bidirectional_status:',
            'release',
        ])
    })

    it('runs Android P5 source through exact foreground ownership and private LAN native start', async () => {
        const events: string[] = []
        let notificationCallbackRan = false
        const foreground = {
            lane: 'p5-source' as const,
            operationId: '55555555-5555-4555-8555-555555555555',
            generation: 15,
        }
        const invoke = vi.fn(async <T>(command: string) => {
            events.push(command)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'source-session', devices: [] } as T
            }
            if (command === 'peer_sync_foreground_source_status') return null as T
            if (command === 'peer_bidirectional_source_reserve') return foreground as T
            if (command === 'peer_bidirectional_start') {
                return { phase: 'running', sessionId: 'source-session', devices: [] } as T
            }
            if (command === 'peer_bidirectional_stop') return foreground as T
            if (command === 'peer_bidirectional_source_release') {
                if (!notificationCallbackRan) return false as T
                return true as T
            }
            if (command === 'peer_bidirectional_status') {
                return { source: { phase: 'stopped', devices: [] } } as T
            }
            throw new Error(`Unexpected command: ${command}`)
        })
        const bridge = {
            startSource: vi.fn(() => { events.push('service-start'); return true }),
            stopSource: vi.fn(() => {
                events.push('service-stop')
                notificationCallbackRan = true
                events.push('native-cancel-callback')
                return true
            }),
        }
        const facade = createPeerBidirectionalFacade({
            platform: 'android',
            invoke: invoke as PeerBidirectionalInvoke,
            runtime: runtime(events),
            bridge,
        })

        await facade.prepare()
        await facade.start('source-session')
        await facade.stop('source-session')

        expect(invoke).toHaveBeenCalledWith('peer_bidirectional_start', {
            sessionId: 'source-session', foreground,
        })
        expect(bridge.startSource).toHaveBeenCalledWith('p5-source', foreground.operationId, 15)
        expect(bridge.stopSource).toHaveBeenCalledWith('p5-source', foreground.operationId, 15)
        expect(invoke).toHaveBeenCalledWith('peer_bidirectional_source_release', {
            sessionId: 'source-session', foreground,
        })
        expect(events.indexOf('service-start')).toBeLessThan(events.indexOf('peer_bidirectional_start'))
        expect(events.indexOf('peer_bidirectional_stop')).toBeLessThan(events.indexOf('service-stop'))
        expect(events.indexOf('native-cancel-callback'))
            .toBeLessThan(events.indexOf('peer_bidirectional_source_release'))
        expect(events.indexOf('service-stop')).toBeLessThan(events.indexOf('peer_bidirectional_source_release'))
    })

    it.each(['false', 'throw'] as const)(
        'retains Android P5 source authority when exact service Stop returns %s, then releases on retry',
        async (failure) => {
            const events: string[] = []
            const foreground = {
                lane: 'p5-source' as const,
                operationId: '56565656-5656-4565-8565-565656565656',
                generation: 23,
            }
            let releaseCalls = 0
            const invoke = vi.fn(async <T>(command: string) => {
                events.push(command)
                if (command === 'peer_bidirectional_prepare') {
                    return { phase: 'prepared', sessionId: 'source-stop-session', devices: [] } as T
                }
                if (command === 'peer_sync_foreground_source_status') return null as T
                if (command === 'peer_bidirectional_source_reserve') return foreground as T
                if (command === 'peer_bidirectional_start') {
                    return { phase: 'running', sessionId: 'source-stop-session', devices: [] } as T
                }
                if (command === 'peer_bidirectional_stop') return foreground as T
                if (command === 'peer_bidirectional_source_release') {
                    releaseCalls += 1
                    return true as T
                }
                if (command === 'peer_bidirectional_status') {
                    return { source: { phase: 'stopped', devices: [] } } as T
                }
                throw new Error(`Unexpected command: ${command}`)
            })
            let stopCalls = 0
            const bridge = {
                startSource: vi.fn(() => true),
                stopSource: vi.fn(() => {
                    stopCalls += 1
                    events.push('service-stop')
                    if (stopCalls === 1 && failure === 'throw') throw new Error('service Stop threw')
                    return stopCalls > 1
                }),
            }
            const facade = createPeerBidirectionalFacade({
                platform: 'android',
                invoke: invoke as PeerBidirectionalInvoke,
                runtime: runtime(events),
                bridge,
            })

            await facade.prepare()
            await facade.start('source-stop-session')
            await expect(facade.stop('source-stop-session')).rejects.toThrow(
                failure === 'throw' ? 'service Stop threw' : 'could not stop',
            )
            expect(releaseCalls).toBe(0)
            await expect(facade.stop('source-stop-session')).resolves.toBeUndefined()
            expect(stopCalls).toBe(2)
            expect(releaseCalls).toBe(1)
            expect(events.indexOf('service-stop')).toBeLessThan(events.indexOf('peer_bidirectional_source_release'))
        },
    )

    it('retains exact Android P5 source ownership when accepted Stop has a stale native release', async () => {
        const events: string[] = []
        const foreground = {
            lane: 'p5-source' as const,
            operationId: '57575757-5757-4575-8575-575757575757',
            generation: 24,
        }
        let releaseCalls = 0
        const invoke = vi.fn(async <T>(command: string) => {
            events.push(command)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'source-stale-session', devices: [] } as T
            }
            if (command === 'peer_sync_foreground_source_status') return null as T
            if (command === 'peer_bidirectional_source_reserve') return foreground as T
            if (command === 'peer_bidirectional_start') {
                return { phase: 'running', sessionId: 'source-stale-session', devices: [] } as T
            }
            if (command === 'peer_bidirectional_stop') return foreground as T
            if (command === 'peer_bidirectional_source_release') {
                releaseCalls += 1
                return (releaseCalls > 1) as T
            }
            if (command === 'peer_bidirectional_status') {
                return { source: { phase: 'stopped', devices: [] } } as T
            }
            throw new Error(`Unexpected command: ${command}`)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'android',
            invoke: invoke as PeerBidirectionalInvoke,
            runtime: runtime(events),
            bridge: {
                startSource: vi.fn(() => true),
                stopSource: vi.fn(() => true),
            },
        })

        await facade.prepare()
        await facade.start('source-stale-session')
        await expect(facade.stop('source-stale-session')).rejects.toThrow('identity is stale')
        await expect(facade.stop('source-stale-session')).resolves.toBeUndefined()
        expect(releaseCalls).toBe(2)
    })

    it('runs Android P5 target to committed refresh before exact service cleanup', async () => {
        const events: string[] = []
        const foreground = {
            lane: 'p5-target' as const,
            operationId: '66666666-6666-4666-8666-666666666666',
            generation: 16,
        }
        const result = {
            kind: 'updated' as const,
            operationId: 'operation-android-p5',
            revision: 8,
            remoteRevision: 9,
            transferredObjects: 1,
            transferredBytes: 12,
            backups: [],
        }
        let nativeOwner = false
        const invoke = vi.fn(async <T>(command: string, args?: Record<string, unknown>) => {
            events.push(command)
            if (command === 'peer_bidirectional_target_foreground_status') return null as T
            if (command === 'peer_bidirectional_target_reserve') {
                nativeOwner = true
                return foreground as T
            }
            if (command === 'peer_bidirectional_sync') {
                expect(args?.foreground).toEqual(foreground)
                return result as T
            }
            if (command === 'peer_bidirectional_target_foreground_release') {
                nativeOwner = false
                return true as T
            }
            throw new Error(`Unexpected command: ${command}`)
        })
        const bridge = {
            startSource: vi.fn(() => { events.push('service-start'); return true }),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const facade = createPeerBidirectionalFacade({
            platform: 'android',
            invoke: invoke as PeerBidirectionalInvoke,
            runtime: runtime(events),
            bridge,
        })

        await expect(facade.sync(pairingUri)).resolves.toEqual(result)
        expect(nativeOwner).toBe(false)
        expect(events).toEqual([
            'peer_bidirectional_target_foreground_status',
            'flush:peer-bidirectional-sync',
            'capture:peer-bidirectional-sync',
            'acquire:7:11',
            'peer_bidirectional_target_reserve',
            'service-start',
            'peer_bidirectional_sync',
            'refresh:8',
            'service-stop',
            'peer_bidirectional_target_foreground_release',
            'release',
        ])
    })

    it('preserves a lost Android response after one committed refresh and exact cleanup', async () => {
        const events: string[] = []
        const foreground = {
            lane: 'p5-target' as const,
            operationId: '69696969-6969-4696-8696-696969696969',
            generation: 19,
        }
        const retained = {
            kind: 'resumeRequired' as const,
            operationId: 'operation-android-lost',
            phase: 'localCommitted' as const,
            committedRevision: 8,
        }
        let foregroundStatusCalls = 0
        const invoke = vi.fn(async <T>(command: string) => {
            events.push(command)
            if (command === 'peer_bidirectional_target_foreground_status') {
                foregroundStatusCalls += 1
                return (foregroundStatusCalls === 1
                    ? null
                    : { foreground, phase: 'terminal', result: retained }) as T
            }
            if (command === 'peer_bidirectional_target_reserve') return foreground as T
            if (command === 'peer_bidirectional_sync') throw new Error('android sync response lost')
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'localCommitted',
                        operationId: retained.operationId,
                        committedRevision: retained.committedRevision,
                    },
                } as T
            }
            if (command === 'peer_bidirectional_target_foreground_release') return true as T
            throw new Error(`Unexpected command: ${command}`)
        })
        const bridge = {
            startSource: vi.fn(() => true),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const facade = createPeerBidirectionalFacade({
            platform: 'android',
            invoke: invoke as PeerBidirectionalInvoke,
            runtime: runtime(events),
            bridge,
        })

        await expect(facade.sync(pairingUri)).rejects.toThrow('android sync response lost')
        expect(events.filter((event) => event === 'refresh:8')).toHaveLength(1)
        expect(events.slice(-4)).toEqual([
            'peer_bidirectional_target_foreground_status',
            'service-stop',
            'peer_bidirectional_target_foreground_release',
            'release',
        ])
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_sync'))
            .toHaveLength(1)
    })

    it.each([
        {
            name: 'resolve',
            command: 'peer_bidirectional_resolve',
            invokeOperation: (facade: ReturnType<typeof createPeerBidirectionalFacade>) => (
                facade.resolve('operation-android-recovered', 'local')
            ),
            operation: {
                phase: 'localCommitted' as const,
                operationId: 'operation-android-recovered',
                committedRevision: 20,
            },
            expected: {
                kind: 'resumeRequired' as const,
                operationId: 'operation-android-recovered',
                phase: 'localCommitted' as const,
                committedRevision: 20,
            },
        },
        {
            name: 'resume',
            command: 'peer_bidirectional_resume',
            invokeOperation: (facade: ReturnType<typeof createPeerBidirectionalFacade>) => (
                facade.resume('operation-android-recovered')
            ),
            operation: {
                phase: 'completed' as const,
                result: {
                    kind: 'updated' as const,
                    operationId: 'operation-android-recovered',
                    revision: 20,
                    remoteRevision: 21,
                    transferredObjects: 1,
                    transferredBytes: 12,
                    backups: [],
                },
            },
            expected: {
                kind: 'updated' as const,
                operationId: 'operation-android-recovered',
                revision: 20,
                remoteRevision: 21,
                transferredObjects: 1,
                transferredBytes: 12,
                backups: [],
            },
        },
    ])('cleans exact Android foreground ownership after a recovered lost $name response', async ({
        command, invokeOperation, operation, expected,
    }) => {
        const events: string[] = []
        const foreground = {
            lane: 'p5-target' as const,
            operationId: '79797979-7979-4797-8797-797979797979',
            generation: 20,
        }
        const invoke = vi.fn(async <T>(nativeCommand: string) => {
            events.push(nativeCommand)
            if (nativeCommand === 'peer_bidirectional_target_foreground_status') return null as T
            if (nativeCommand === 'peer_bidirectional_target_reserve') return foreground as T
            if (nativeCommand === command) throw new Error(`${command} response lost`)
            if (nativeCommand === 'peer_bidirectional_status') {
                return { source: { phase: 'idle', devices: [] }, operation } as T
            }
            if (nativeCommand === 'peer_bidirectional_target_foreground_release') return true as T
            throw new Error(`Unexpected command: ${nativeCommand}`)
        })
        const bridge = {
            startSource: vi.fn(() => true),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const facade = createPeerBidirectionalFacade({
            platform: 'android',
            invoke: invoke as PeerBidirectionalInvoke,
            runtime: runtime(events),
            bridge,
        })

        await expect(invokeOperation(facade)).resolves.toEqual(expected)
        expect(events.filter((event) => event === command)).toHaveLength(1)
        expect(events.filter((event) => event === 'refresh:20')).toHaveLength(1)
        expect(events.filter((event) => event === 'service-stop')).toHaveLength(1)
        expect(events.filter((event) => event === 'peer_bidirectional_target_foreground_release'))
            .toHaveLength(1)
    })

    it.each([
        ['resolve', 'peer_bidirectional_resolve', (facade: ReturnType<typeof createPeerBidirectionalFacade>) => (
            facade.resolve('operation-android-cleanup-error', 'local')
        ), {
            phase: 'localCommitted' as const,
            operationId: 'operation-android-cleanup-error',
            committedRevision: 22,
        }],
        ['resume', 'peer_bidirectional_resume', (facade: ReturnType<typeof createPeerBidirectionalFacade>) => (
            facade.resume('operation-android-cleanup-error')
        ), {
            phase: 'completed' as const,
            result: {
                kind: 'noChanges' as const,
                operationId: 'operation-android-cleanup-error',
                revision: 22,
                remoteRevision: 22,
                transferredObjects: 0,
                transferredBytes: 0,
                backups: [],
            },
        }],
    ] as const)(
        'retains the lost Android %s response and exact cleanup failure without retrying either',
        async (_name, command, invokeOperation, operation) => {
            const events: string[] = []
            const foreground = {
                lane: 'p5-target' as const,
                operationId: '89898989-8989-4898-8898-898989898989',
                generation: 22,
            }
            const primary = new Error(`${command} response lost`)
            const invoke = vi.fn(async <T>(nativeCommand: string) => {
                events.push(nativeCommand)
                if (nativeCommand === 'peer_bidirectional_target_foreground_status') return null as T
                if (nativeCommand === 'peer_bidirectional_target_reserve') return foreground as T
                if (nativeCommand === command) throw primary
                if (nativeCommand === 'peer_bidirectional_status') {
                    return { source: { phase: 'idle', devices: [] }, operation } as T
                }
                throw new Error(`Unexpected command: ${nativeCommand}`)
            })
            const cleanup = new Error('Android bidirectional foreground service could not stop')
            const facade = createPeerBidirectionalFacade({
                platform: 'android',
                invoke: invoke as PeerBidirectionalInvoke,
                runtime: runtime(events),
                bridge: {
                    startSource: vi.fn(() => true),
                    stopSource: vi.fn(() => false),
                },
            })

            const error = await invokeOperation(facade).catch((cause) => cause)
            expect(error).toBeInstanceOf(AggregateError)
            expect((error as AggregateError).errors).toEqual([primary, cleanup])
            expect(events.filter((event) => event === command)).toHaveLength(1)
            expect(events.filter((event) => event === 'refresh:22')).toHaveLength(1)
            expect(events.filter((event) => event === 'peer_bidirectional_target_foreground_status'))
                .toHaveLength(1)
            expect(events).not.toContain('peer_bidirectional_target_foreground_release')
        },
    )

    it('recovers a cancelled Android P5 Running operation from durable LocalCommitted authority', async () => {
        const events: string[] = []
        const foreground = {
            lane: 'p5-target' as const,
            operationId: '77777777-7777-4777-8777-777777777777',
            generation: 17,
        }
        const result = {
            kind: 'resumeRequired' as const,
            operationId: 'operation-retained',
            phase: 'localCommitted' as const,
            committedRevision: 12,
        }
        let statusCalls = 0
        const invoke = vi.fn(async <T>(command: string) => {
            events.push(command)
            if (command === 'peer_bidirectional_target_foreground_status') {
                statusCalls += 1
                return (statusCalls === 1
                    ? { foreground, phase: 'running' }
                    : { foreground, phase: 'terminal', result }) as T
            }
            if (command === 'peer_bidirectional_target_foreground_cancel') return true as T
            if (command === 'peer_bidirectional_target_foreground_release') return true as T
            throw new Error(`Unexpected command: ${command}`)
        })
        const bridge = {
            startSource: vi.fn(() => true),
            stopSource: vi.fn(() => { events.push('service-stop'); return true }),
        }
        const facade = createPeerBidirectionalFacade({
            platform: 'android',
            invoke: invoke as PeerBidirectionalInvoke,
            runtime: runtime(events),
            bridge,
        })

        await facade.recoverTargetForeground?.()
        expect(events).toEqual([
            'peer_bidirectional_target_foreground_status',
            'peer_bidirectional_target_foreground_cancel',
            'peer_bidirectional_target_foreground_status',
            'flush:peer-bidirectional-target-recovery',
            'capture:peer-bidirectional-target-recovery',
            'acquire:7:11',
            'refresh:12',
            'release',
            'service-stop',
            'peer_bidirectional_target_foreground_release',
        ])
        expect(invoke).not.toHaveBeenCalledWith('peer_bidirectional_acknowledge', expect.anything())
    })

    it.each([
        {
            name: 'AwaitingConflict',
            result: {
                kind: 'conflict' as const,
                operationId: 'operation-conflict',
                conflicts: [{ key: 'root', type: 'sameRecord' as const }],
                localManifestHash: 'a'.repeat(64),
                remoteManifestHash: 'b'.repeat(64),
            },
            refreshedRevision: undefined,
        },
        {
            name: 'Completed',
            result: {
                kind: 'noChanges' as const,
                operationId: 'operation-completed',
                revision: 14,
                remoteRevision: 14,
                transferredObjects: 0,
                transferredBytes: 0,
                backups: [],
            },
            refreshedRevision: 14,
        },
    ])('recovers Android P5 $name without notification ACK or abandon', async ({ result, refreshedRevision }) => {
        const events: string[] = []
        const foreground = {
            lane: 'p5-target' as const,
            operationId: '88888888-8888-4888-8888-888888888888',
            generation: 18,
        }
        const invoke = vi.fn(async <T>(command: string) => {
            events.push(command)
            if (command === 'peer_bidirectional_target_foreground_status') {
                return { foreground, phase: 'terminal', result } as T
            }
            if (command === 'peer_bidirectional_target_foreground_release') return true as T
            throw new Error(`Unexpected command: ${command}`)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'android',
            invoke: invoke as PeerBidirectionalInvoke,
            runtime: runtime(events),
            bridge: {
                startSource: vi.fn(() => true),
                stopSource: vi.fn(() => true),
            },
        })

        await facade.recoverTargetForeground?.()
        expect(events.filter((event) => event.startsWith('refresh:'))).toEqual(
            refreshedRevision === undefined ? [] : [`refresh:${refreshedRevision}`],
        )
        expect(events).not.toContain('peer_bidirectional_acknowledge')
        expect(events).not.toContain('peer_sync_foreground_source_abandon')
    })

    it.each(['local', 'remote'] as const)(
        'resolves the %s winner with a fresh public link in one native mutation',
        async (winner) => {
            const events: string[] = []
            const result = {
                kind: 'conflict' as const,
                operationId: 'operation-public-conflict',
                conflicts: [{ key: 'r1:root', type: 'sameRecord' as const }],
                localManifestHash: 'c'.repeat(64),
                remoteManifestHash: 'd'.repeat(64),
            }
            const invoke = vi.fn(async () => result)
            const facade = createPeerBidirectionalFacade({
                platform: 'desktop',
                runtime: runtime(events),
                invoke: invoke as unknown as PeerBidirectionalInvoke,
            })

            await expect(facade.resolve('operation-public-conflict', winner, publicPairingUri))
                .resolves.toEqual(result)
            expect(invoke).toHaveBeenCalledWith('peer_bidirectional_resolve_with_link', {
                operationId: 'operation-public-conflict',
                winner,
                endpoint: 'https://fresh-public.example',
                sessionId: '123e4567-e89b-42d3-a456-426614174000',
                manifestId: 'a'.repeat(64),
                claim: 'b'.repeat(64),
                expectedRevision: 7,
            })
        },
    )

    it('rejects a malformed linked conflict source before invoking native authority', async () => {
        const invoke = vi.fn()
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime([]),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        expect(() => facade.resolve('operation-public-conflict', 'local', 'not-a-pairing-link'))
            .toThrow('Invalid peer sync pairing URI')
        expect(invoke).not.toHaveBeenCalled()
    })

    it('starts Quick and Named P5 tunnels through transient one-shot commands', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_tunnel_start') {
                return { phase: 'running', sessionId: 'session-source', devices: [], tunnel: { kind: 'quick' } }
            }
            return undefined
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime([]),
        })
        await facade.prepare()

        await facade.startQuickTunnel('session-source')
        await facade.startNamedTunnel('session-source', 'transient-secret', 'https://sync.example.com')

        expect(invoke).toHaveBeenCalledWith('peer_bidirectional_tunnel_start', {
            sessionId: 'session-source',
            tunnel: { kind: 'quick' },
        })
        expect(invoke).toHaveBeenCalledWith('peer_bidirectional_tunnel_start', {
            sessionId: 'session-source',
            tunnel: {
                kind: 'named',
                token: 'transient-secret',
                expectedPublicBaseUrl: 'https://sync.example.com',
            },
        })
    })

    it('accepts a strict bare public HTTPS P5 link only on desktop', () => {
        const publicPairing = pairingUri.replace(
            'http%3A%2F%2F192.168.1.20%3A32146',
            'https%3A%2F%2Fquick-id.trycloudflare.com',
        )
        expect(parsePeerBidirectionalUri(publicPairing).endpoint)
            .toBe('https://quick-id.trycloudflare.com')
        for (const invalid of [
            publicPairing.replace('https%3A', 'http%3A'),
            publicPairing.replace('trycloudflare.com', '127.0.0.1'),
            publicPairing.replace('trycloudflare.com', 'trycloudflare.com%2Fpath'),
        ]) expect(() => parsePeerBidirectionalUri(invalid)).toThrow('Invalid peer sync pairing URI')
    })

    it.each(['', 'session-source'])(
        'forwards the existing string session sentinel when revoking a device',
        async (sessionId) => {
            const invoke = vi.fn(async () => undefined)
            const facade = createPeerBidirectionalFacade({
                platform: 'desktop',
                invoke: invoke as unknown as PeerBidirectionalInvoke,
            })

            await facade.revoke(sessionId, 'device-durable')

            expect(invoke).toHaveBeenCalledWith('peer_bidirectional_revoke', {
                sessionId,
                deviceId: 'device-durable',
            })
        },
    )

    it('rejects a target mutation while source preparation is in flight', async () => {
        const events: string[] = []
        let finishPrepare!: (value: unknown) => void
        const invoke = vi.fn((command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return new Promise((resolve) => {
                    finishPrepare = resolve
                })
            }
            return Promise.resolve(undefined)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        const preparing = facade.prepare()
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith(
            'peer_bidirectional_prepare',
            { expectedRevision: 7 },
        ))
        await expect(facade.sync(pairingUri)).rejects.toThrow('peer sync source is active')
        finishPrepare({ phase: 'prepared', sessionId: 'session-source', devices: [] })
        await preparing
    })

    it('rejects source preparation while a target mutation is in flight', async () => {
        const events: string[] = []
        let finishSync!: (value: unknown) => void
        const invoke = vi.fn((command: string) => {
            if (command === 'peer_bidirectional_sync') {
                return new Promise((resolve) => {
                    finishSync = resolve
                })
            }
            return Promise.resolve(undefined)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        const syncing = facade.sync(pairingUri)
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith(
            'peer_bidirectional_sync',
            expect.objectContaining({ expectedRevision: 7 }),
        ))
        await expect(facade.prepare()).rejects.toThrow('source or target is already active')
        finishSync({
            kind: 'noChanges',
            operationId: 'operation-target-active',
            revision: 7,
            remoteRevision: 7,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        })
        await syncing
    })

    it('does not start a source host without its prepared mutation fence', async () => {
        const invoke = vi.fn(async () => ({
            phase: 'running' as const,
            sessionId: 'session-unfenced',
            devices: [],
        }))
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime([]),
        })

        await expect(facade.start('session-unfenced')).rejects.toThrow('not prepared')
        expect(invoke).not.toHaveBeenCalled()
    })

    it('refreshes a source-side committed revision once and retains its fence until stop', async () => {
        const events: string[] = []
        const completed = {
            phase: 'completed' as const,
            result: {
                kind: 'updated' as const,
                operationId: 'operation-source-complete',
                revision: 8,
                remoteRevision: 9,
                transferredObjects: 2,
                transferredBytes: 12,
                backups: [{ packageId: 'backup-source', side: 'local' as const, path: 'source.risulossless' }],
            },
        }
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'running', sessionId: 'session-source', devices: [] },
                    operation: completed,
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await facade.prepare()
        await facade.status()
        await facade.status()
        expect(events.filter((event) => event === 'refresh:8')).toHaveLength(1)
        expect(events).not.toContain('release')

        await facade.stop('session-source')
        expect(events.at(-1)).toBe('release')
    })

    it('retries only a failed source refresh before polling native status again', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'running', sessionId: 'session-source', devices: [] },
                    operation: {
                        phase: 'completed',
                        result: {
                            kind: 'noChanges',
                            operationId: 'operation-source-refresh',
                            revision: 7,
                            remoteRevision: 7,
                            transferredObjects: 0,
                            transferredBytes: 0,
                            backups: [],
                        },
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('source renderer refresh failed')
                            }
                        },
                    }
                },
            },
        })

        await facade.prepare()
        await expect(facade.status()).rejects.toThrow('source renderer refresh failed')
        await expect(facade.status()).resolves.toMatchObject({ operation: { phase: 'completed' } })
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_status')).toHaveLength(1)
        expect(events.filter((event) => event === 'refresh:7')).toHaveLength(2)
        expect(events).not.toContain('release')
    })

    it('closes the source host before finishing a pending refresh and releasing its fence', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'running', sessionId: 'session-source', devices: [] },
                    operation: {
                        phase: 'completed',
                        result: {
                            kind: 'updated',
                            operationId: 'operation-stop-refresh',
                            revision: 8,
                            remoteRevision: 9,
                            transferredObjects: 1,
                            transferredBytes: 12,
                            backups: [],
                        },
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('source refresh failed')
                            }
                        },
                    }
                },
            },
        })

        await facade.prepare()
        await expect(facade.status()).rejects.toThrow('source refresh failed')
        await facade.stop('session-source')
        expect(events.slice(-4)).toEqual([
            'invoke:peer_bidirectional_stop',
            'invoke:peer_bidirectional_status',
            'refresh:8',
            'release',
        ])
    })

    it('checks for a newly completed source revision after closing the host and before release', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'stopped', sessionId: 'session-source', devices: [] },
                    operation: {
                        phase: 'completed',
                        result: {
                            kind: 'updated',
                            operationId: 'operation-new-at-stop',
                            revision: 8,
                            remoteRevision: 9,
                            transferredObjects: 1,
                            transferredBytes: 12,
                            backups: [],
                        },
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await facade.prepare()
        await facade.stop('session-source')
        expect(events.slice(-4)).toEqual([
            'invoke:peer_bidirectional_stop',
            'invoke:peer_bidirectional_status',
            'refresh:8',
            'release',
        ])
    })

    it('coalesces polling and stop onto one exact source refresh', async () => {
        const events: string[] = []
        let finishRefresh!: () => void
        let refreshCalls = 0
        const base = runtime(events)
        const completedStatus = {
            source: { phase: 'running' as const, sessionId: 'session-source', devices: [] },
            operation: {
                phase: 'completed' as const,
                result: {
                    kind: 'updated' as const,
                    operationId: 'operation-coalesced-refresh',
                    revision: 8,
                    remoteRevision: 9,
                    transferredObjects: 1,
                    transferredBytes: 12,
                    backups: [],
                },
            },
        }
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') return completedStatus
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            refreshCalls += 1
                            await new Promise<void>((resolve) => {
                                finishRefresh = resolve
                            })
                        },
                    }
                },
            },
        })

        await facade.prepare()
        const polling = facade.status()
        await vi.waitFor(() => expect(refreshCalls).toBe(1))
        const stopping = facade.stop('session-source')
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith(
            'peer_bidirectional_stop',
            { sessionId: 'session-source' },
        ))
        expect(refreshCalls).toBe(1)
        finishRefresh()
        await Promise.all([polling, stopping])
        expect(refreshCalls).toBe(1)
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
    })

    it('finishes a newer stop-time revision after an older source refresh in flight', async () => {
        const events: string[] = []
        let finishFirstRefresh!: () => void
        let statusCalls = 0
        const base = runtime(events)
        const statusForRevision = (revision: number) => ({
            source: { phase: 'running' as const, sessionId: 'session-source', devices: [] },
            operation: {
                phase: 'completed' as const,
                result: {
                    kind: 'updated' as const,
                    operationId: 'operation-sequenced-refresh',
                    revision,
                    remoteRevision: 9,
                    transferredObjects: 1,
                    transferredBytes: 12,
                    backups: [],
                },
            },
        })
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                statusCalls += 1
                return statusForRevision(statusCalls === 1 ? 8 : 9)
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (revision === 8) {
                                await new Promise<void>((resolve) => {
                                    finishFirstRefresh = resolve
                                })
                            }
                        },
                    }
                },
            },
        })

        await facade.prepare()
        const polling = facade.status()
        await vi.waitFor(() => expect(events).toContain('refresh:8'))
        const stopping = facade.stop('session-source')
        await vi.waitFor(() => expect(statusCalls).toBe(2))
        finishFirstRefresh()
        await Promise.all([polling, stopping])
        expect(events.filter((event) => event.startsWith('refresh:'))).toEqual([
            'refresh:8',
            'refresh:9',
        ])
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
    })

    it('does not execute a delayed lower revision after stop observes a higher revision', async () => {
        const events: string[] = []
        let finishDelayedStatus!: (value: unknown) => void
        let finishHigherRefresh!: () => void
        let statusCalls = 0
        const base = runtime(events)
        const statusForRevision = (revision: number) => ({
            source: { phase: 'running' as const, sessionId: 'session-source', devices: [] },
            operation: {
                phase: 'completed' as const,
                result: {
                    kind: 'updated' as const,
                    operationId: 'operation-high-water',
                    revision,
                    remoteRevision: 9,
                    transferredObjects: 1,
                    transferredBytes: 12,
                    backups: [],
                },
            },
        })
        const invoke = vi.fn((command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return Promise.resolve({ phase: 'prepared', sessionId: 'session-source', devices: [] })
            }
            if (command === 'peer_bidirectional_status') {
                statusCalls += 1
                if (statusCalls === 1) {
                    return new Promise((resolve) => {
                        finishDelayedStatus = resolve
                    })
                }
                return Promise.resolve(statusForRevision(9))
            }
            return Promise.resolve(undefined)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (revision === 9) {
                                await new Promise<void>((resolve) => {
                                    finishHigherRefresh = resolve
                                })
                            }
                        },
                    }
                },
            },
        })

        await facade.prepare()
        const delayedPoll = facade.status()
        await vi.waitFor(() => expect(statusCalls).toBe(1))
        const stopping = facade.stop('session-source')
        await vi.waitFor(() => expect(events).toContain('refresh:9'))
        finishDelayedStatus(statusForRevision(8))
        await Promise.resolve()
        finishHigherRefresh()
        await Promise.all([delayedPoll, stopping])
        expect(events.filter((event) => event.startsWith('refresh:'))).toEqual(['refresh:9'])
    })

    it('holds the fence until mandatory post-stop status is fetched', async () => {
        const events: string[] = []
        let finishPreStopRefresh!: () => void
        let finishStopStatus!: (value: unknown) => void
        let statusCalls = 0
        const base = runtime(events)
        const statusForRevision = (revision: number) => ({
            source: { phase: 'running' as const, sessionId: 'session-source', devices: [] },
            operation: {
                phase: 'completed' as const,
                result: {
                    kind: 'updated' as const,
                    operationId: 'operation-stop-status-gate',
                    revision,
                    remoteRevision: 9,
                    transferredObjects: 1,
                    transferredBytes: 12,
                    backups: [],
                },
            },
        })
        const invoke = vi.fn((command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return Promise.resolve({ phase: 'prepared', sessionId: 'session-source', devices: [] })
            }
            if (command === 'peer_bidirectional_status') {
                statusCalls += 1
                if (statusCalls === 1) return Promise.resolve(statusForRevision(8))
                return new Promise((resolve) => {
                    finishStopStatus = resolve
                })
            }
            return Promise.resolve(undefined)
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (revision === 8) {
                                await new Promise<void>((resolve) => {
                                    finishPreStopRefresh = resolve
                                })
                            }
                        },
                    }
                },
            },
        })

        await facade.prepare()
        const polling = facade.status()
        await vi.waitFor(() => expect(events).toContain('refresh:8'))
        const stopping = facade.stop('session-source')
        await vi.waitFor(() => expect(statusCalls).toBe(2))
        finishPreStopRefresh()
        await polling
        expect(events).not.toContain('release')
        finishStopStatus(statusForRevision(9))
        await stopping
        expect(events.slice(-2)).toEqual(['refresh:9', 'release'])
    })

    it('releases a stopped source fence after repeated refresh failures eventually recover', async () => {
        const events: string[] = []
        let refreshCalls = 0
        const base = runtime(events)
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_prepare') {
                return { phase: 'prepared', sessionId: 'session-source', devices: [] }
            }
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'stopped', sessionId: 'session-source', devices: [] },
                    operation: {
                        phase: 'completed',
                        result: {
                            kind: 'updated',
                            operationId: 'operation-eventual-refresh',
                            revision: 8,
                            remoteRevision: 9,
                            transferredObjects: 1,
                            transferredBytes: 12,
                            backups: [],
                        },
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            refreshCalls += 1
                            if (refreshCalls < 3) throw new Error(`refresh failed ${refreshCalls}`)
                        },
                    }
                },
            },
        })

        await facade.prepare()
        await expect(facade.stop('session-source')).rejects.toThrow('refresh failed 1')
        await expect(facade.status()).rejects.toThrow('refresh failed 2')
        expect(events).not.toContain('release')
        await expect(facade.status()).resolves.toMatchObject({ operation: { phase: 'completed' } })
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_stop')).toHaveLength(1)
    })

    it('keeps production disabled unless every native readiness field is true', async () => {
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: (async () => ({
                desktop: true,
                sourceReady: true,
                atomicActivationReady: true,
                authenticatedTransportReady: false,
                losslessBackupReady: true,
                durableStateReady: true,
                productionEnabled: true,
            })) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.capabilities()).resolves.toMatchObject({ productionEnabled: false })
    })

    it('holds the mutation fence through native completion and renderer refresh', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${String(args?.expectedRevision ?? '')}`)
            return {
                kind: 'updated',
                operationId: 'operation-1',
                revision: 8,
                remoteRevision: 4,
                transferredObjects: 2,
                transferredBytes: 19,
                backups: [],
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await expect(facade.sync(pairingUri)).resolves.toMatchObject({ kind: 'updated', revision: 8 })
        expect(events).toEqual([
            'flush:peer-bidirectional-sync',
            'capture:peer-bidirectional-sync',
            'acquire:7:11',
            'invoke:peer_bidirectional_sync:7',
            'refresh:8',
            'release',
        ])
    })

    it.each([
        {
            kind: 'resumeRequired' as const,
            operationId: 'operation-local-committed',
            phase: 'localCommitted' as const,
            committedRevision: 8,
        },
        {
            kind: 'sourceUnavailable' as const,
            operationId: 'operation-source-unavailable',
            committedRevision: 8,
        },
    ])('refreshes the committed working set before returning $kind', async (result) => {
        const events: string[] = []
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: (async () => result) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resume(result.operationId)).resolves.toBe(result)
        expect(events.slice(-2)).toEqual(['refresh:8', 'release'])
    })

    it('returns the recovered durable local commit after a lost resolve response', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_resolve') throw new Error('native response lost')
            if (command === 'peer_bidirectional_status') {
                return {
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-response-loss',
                        committedRevision: 8,
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolve('operation-response-loss', 'local')).resolves.toEqual({
            kind: 'resumeRequired',
            operationId: 'operation-response-loss',
            phase: 'localCommitted',
            committedRevision: 8,
        })
        expect(events.slice(-3)).toEqual([
            'invoke:peer_bidirectional_status',
            'refresh:8',
            'release',
        ])
    })

    it.each([
        {
            phase: 'localCommitted' as const,
            operation: {
                phase: 'localCommitted' as const,
                operationId: 'operation-linked-response-loss',
                committedRevision: 8,
            },
            expected: {
                kind: 'resumeRequired' as const,
                operationId: 'operation-linked-response-loss',
                phase: 'localCommitted' as const,
                committedRevision: 8,
            },
        },
        {
            phase: 'completed' as const,
            operation: {
                phase: 'completed' as const,
                result: {
                    kind: 'updated' as const,
                    operationId: 'operation-linked-response-loss',
                    revision: 8,
                    remoteRevision: 9,
                    transferredObjects: 1,
                    transferredBytes: 12,
                    backups: [],
                },
            },
            expected: {
                kind: 'updated' as const,
                operationId: 'operation-linked-response-loss',
                revision: 8,
                remoteRevision: 9,
                transferredObjects: 1,
                transferredBytes: 12,
                backups: [],
            },
        },
    ])('returns exact linked resolve progress after a lost $phase response', async ({ operation, expected }) => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_resolve_with_link') throw new Error('linked response lost')
            if (command === 'peer_bidirectional_status') {
                return { source: { phase: 'idle', devices: [] }, operation }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolve(
            'operation-linked-response-loss',
            'local',
            publicPairingUri,
        )).resolves.toEqual(expected)
        expect(events).toContain('refresh:8')
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_resolve_with_link'))
            .toHaveLength(1)
    })

    it.each([
        {
            name: 'mismatched operation',
            operation: {
                phase: 'localCommitted' as const,
                operationId: 'another-operation',
                committedRevision: 8,
            },
        },
        {
            name: 'unchanged conflict',
            operation: {
                phase: 'awaitingConflict' as const,
                result: {
                    kind: 'conflict' as const,
                    operationId: 'operation-linked-rejected',
                    conflicts: [{ key: 'r1:root', type: 'sameRecord' as const }],
                    localManifestHash: 'c'.repeat(64),
                    remoteManifestHash: 'd'.repeat(64),
                },
            },
        },
    ])('preserves linked resolve rejection for $name', async ({ operation }) => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_resolve_with_link') throw new Error('linked resolve rejected')
            if (command === 'peer_bidirectional_status') {
                return { source: { phase: 'idle', devices: [] }, operation }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime([]),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolve(
            'operation-linked-rejected',
            'remote',
            publicPairingUri,
        )).rejects.toThrow('linked resolve rejected')
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_resolve_with_link'))
            .toHaveLength(1)
    })

    it.each([
        {
            phase: 'localCommitted' as const,
            operationId: 'operation-normalized-local',
            committedRevision: 8,
        },
        {
            phase: 'completed' as const,
            result: {
                kind: 'updated' as const,
                operationId: 'operation-normalized-completed',
                revision: 8,
                remoteRevision: 9,
                transferredObjects: 1,
                transferredBytes: 12,
                backups: [],
            },
        },
    ])(
        'refreshes a status-normalized $phase sync commit without claiming a lost response as success',
        async (operation) => {
            const events: string[] = []
            const invoke = vi.fn(async (command: string) => {
                events.push(`invoke:${command}`)
                if (command === 'peer_bidirectional_sync') throw new Error('sync response lost')
                if (command === 'peer_bidirectional_status') {
                    return {
                        source: { phase: 'idle', devices: [] },
                        operation,
                    }
                }
            })
            const facade = createPeerBidirectionalFacade({
                platform: 'desktop',
                runtime: runtime(events),
                invoke: invoke as unknown as PeerBidirectionalInvoke,
            })

            await expect(facade.sync(pairingUri)).rejects.toThrow('sync response lost')
            expect(events.slice(-3)).toEqual([
                'invoke:peer_bidirectional_status',
                'refresh:8',
                'release',
            ])
        },
    )

    it.each(['sourcePrepared', 'targetPrepared'] as const)(
        'does not refresh a proven-precommit %s status after a failed mutation',
        async (phase) => {
            const events: string[] = []
            const invoke = vi.fn(async (command: string) => {
                events.push(`invoke:${command}`)
                if (command === 'peer_bidirectional_sync') throw new Error('sync failed before commit')
                if (command === 'peer_bidirectional_status') {
                    return {
                        source: { phase: 'idle', devices: [] },
                        operation: { phase, operationId: `operation-${phase}` },
                    }
                }
            })
            const facade = createPeerBidirectionalFacade({
                platform: 'desktop',
                runtime: runtime(events),
                invoke: invoke as unknown as PeerBidirectionalInvoke,
            })

            await expect(facade.sync(pairingUri)).rejects.toThrow('sync failed before commit')

            expect(events.filter((event) => event.startsWith('refresh:'))).toHaveLength(0)
            expect(events.at(-1)).toBe('release')
        },
    )

    it('does not suppress a failed resolve when the awaiting-conflict status is unchanged', async () => {
        const events: string[] = []
        const conflict = {
            phase: 'awaitingConflict' as const,
            result: {
                kind: 'conflict' as const,
                operationId: 'operation-unchanged-conflict',
                conflicts: [{ key: 'r1:root', type: 'sameRecord' as const }],
                localManifestHash: 'a'.repeat(64),
                remoteManifestHash: 'b'.repeat(64),
            },
        }
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_resolve') throw new Error('backup creation failed')
            if (command === 'peer_bidirectional_status') {
                return { source: { phase: 'idle', devices: [] }, operation: conflict }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolve('operation-unchanged-conflict', 'remote')).rejects.toThrow('backup creation failed')
        expect(events.filter((event) => event.startsWith('refresh:'))).toHaveLength(0)
    })

    it('returns explicit same-record conflicts without refreshing the renderer', async () => {
        const events: string[] = []
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: (async () => ({
                kind: 'conflict',
                operationId: 'operation-2',
                conflicts: [
                    { key: 'r1:root', type: 'sameRecord' },
                    { key: 'r1:preset:WyIwIl0', type: 'deleteVsEdit' },
                ],
                localManifestHash: 'c'.repeat(64),
                remoteManifestHash: 'd'.repeat(64),
            })) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.sync(pairingUri)).resolves.toMatchObject({
            kind: 'conflict',
            operationId: 'operation-2',
        })
        expect(events).not.toContain('refresh:7')
        expect(events.filter((event) => event.startsWith('refresh:'))).toHaveLength(0)
        expect(events.at(-1)).toBe('release')
    })

    it('does not refresh the renderer for a stale generation result', async () => {
        const events: string[] = []
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: (async () => ({
                kind: 'stale',
                operationId: 'operation-stale',
                reason: 'remoteGeneration',
            })) as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.sync(pairingUri)).resolves.toMatchObject({ kind: 'stale' })
        expect(events.filter((event) => event.startsWith('refresh:'))).toHaveLength(0)
        expect(events.at(-1)).toBe('release')
    })

    it('retries only renderer refresh after native completion was committed', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async () => ({
            kind: 'noChanges' as const,
            operationId: 'operation-3',
            revision: 7,
            remoteRevision: 5,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }))
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('renderer refresh failed')
                            }
                        },
                    }
                },
            },
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.sync(pairingUri)).rejects.toThrow('renderer refresh failed')
        await expect(facade.sync(pairingUri)).resolves.toMatchObject({ kind: 'noChanges' })
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
    })

    it('resolves a retained conflict by operation id and explicit winner', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${String(args?.operationId)}:${String(args?.winner)}`)
            return {
                kind: 'updated',
                operationId: 'operation-4',
                revision: 9,
                remoteRevision: 6,
                transferredObjects: 1,
                transferredBytes: 8,
                backups: [{ packageId: 'backup-1', side: 'remote', path: 'conflict.risulossless' }],
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: runtime(events),
        })

        await facade.resolve('operation-4', 'local')
        expect(events).toContain('invoke:peer_bidirectional_resolve:operation-4:local')
        expect(events).toContain('refresh:9')
    })

    it('retains the exact resolve result and fence when renderer refresh fails', async () => {
        const events: string[] = []
        let failRefresh = true
        const base = runtime(events)
        const invoke = vi.fn(async () => ({
            kind: 'updated' as const,
            operationId: 'operation-refresh',
            revision: 9,
            remoteRevision: 6,
            transferredObjects: 1,
            transferredBytes: 8,
            backups: [],
        }))
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: invoke as unknown as PeerBidirectionalInvoke,
            runtime: {
                ...base,
                async acquirePersistentMutationFence(token) {
                    const fence = await base.acquirePersistentMutationFence(token)
                    return {
                        ...fence,
                        async refreshCommittedWorkingSet(revision) {
                            events.push(`refresh:${revision}`)
                            if (failRefresh) {
                                failRefresh = false
                                throw new Error('renderer refresh failed')
                            }
                        },
                    }
                },
            },
        })

        await expect(facade.resolve('operation-refresh', 'local')).rejects.toBeInstanceOf(PeerBidirectionalRefreshError)
        await expect(facade.resolve('operation-refresh', 'remote')).rejects.toThrow('different peer sync retry')
        await expect(facade.resolve('operation-refresh', 'local')).resolves.toMatchObject({ kind: 'updated' })
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
    })
})
