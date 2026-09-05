import { describe, expect, it, vi } from 'vitest'

import {
    createPeerBidirectionalFacade,
    PeerBidirectionalRefreshError,
    type PeerBidirectionalInvoke,
    type PeerBidirectionalMutationRuntime,
} from './peerBidirectional'

function runtime(log: string[]): PeerBidirectionalMutationRuntime {
    return {
        async flushPendingData(reason) {
            log.push(`flush:${reason}`)
        },
        async capturePersistentMutationToken(reason) {
            log.push(`capture:${reason}`)
            return { revision: 7, mutationGeneration: 11 }
        },
        async acquireDestructiveReplacementFence(token) {
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
    it.each(['sourcePrepared', 'targetPrepared'] as const)(
        'preserves a durable %s operation in status',
        async (phase) => {
            const status = { operation: { phase, operationId: `operation-${phase}` } }
            const nativeInvoke = vi.fn(async (command: string) => {
                if (command === 'peer_bidirectional_status') return status
                throw new Error(`Unexpected command: ${command}`)
            })
            const peer = createPeerBidirectionalFacade({
                platform: 'desktop',
                invoke: nativeInvoke as unknown as PeerBidirectionalInvoke,
            })

            await expect(peer.status()).resolves.toEqual(status)
        },
    )

    it('does not create a fallback authority on web', async () => {
        const platform = 'web' as const
        const facade = createPeerBidirectionalFacade({ platform })
        await expect(facade.capabilities()).rejects.toThrow(`Peer sync is unsupported on ${platform}`)
        await expect(facade.syncRegistered('device-web')).rejects.toThrow(
            `Peer sync is unsupported on ${platform}`,
        )
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
            if (command === 'peer_bidirectional_sync_registered') {
                expect(args?.deviceId).toBe('device-android-p5')
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

        await expect(facade.syncRegistered('device-android-p5')).resolves.toEqual(result)
        expect(nativeOwner).toBe(false)
        expect(events).toEqual([
            'peer_bidirectional_target_foreground_status',
            'flush:peer-bidirectional-sync',
            'capture:peer-bidirectional-sync',
            'acquire:7:11',
            'peer_bidirectional_target_reserve',
            'service-start',
            'peer_bidirectional_sync_registered',
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
            if (command === 'peer_bidirectional_sync_registered') {
                throw new Error('android sync response lost')
            }
            if (command === 'peer_bidirectional_status') {
                return {
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

        await expect(facade.syncRegistered('device-android-lost')).rejects.toThrow('android sync response lost')
        expect(events.filter((event) => event === 'refresh:8')).toHaveLength(1)
        expect(events.slice(-4)).toEqual([
            'peer_bidirectional_target_foreground_status',
            'service-stop',
            'peer_bidirectional_target_foreground_release',
            'release',
        ])
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_sync_registered'))
            .toHaveLength(1)
    })

    it.each([
        {
            name: 'resolve',
            command: 'peer_bidirectional_resolve_registered',
            invokeOperation: (facade: ReturnType<typeof createPeerBidirectionalFacade>) => (
                facade.resolveRegistered('device-android', 'operation-android-recovered', 'local')
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
                return { operation } as T
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
        ['resolve', 'peer_bidirectional_resolve_registered', (facade: ReturnType<typeof createPeerBidirectionalFacade>) => (
            facade.resolveRegistered('device-android', 'operation-android-cleanup-error', 'local')
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
                    return { operation } as T
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

    it('keeps production disabled unless every native readiness field is true', async () => {
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            invoke: (async () => ({
                desktop: true,
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

        await expect(facade.syncRegistered('device-fence')).resolves.toMatchObject({
            kind: 'updated',
            revision: 8,
        })
        expect(events).toEqual([
            'flush:peer-bidirectional-sync',
            'capture:peer-bidirectional-sync',
            'acquire:7:11',
            'invoke:peer_bidirectional_sync_registered:7',
            'refresh:8',
            'release',
        ])
    })

    it('refuses a second registered mutation while the first native call is pending', async () => {
        const events: string[] = []
        let releaseNative: () => void = () => undefined
        const nativePending = new Promise<void>((resolve) => { releaseNative = resolve })
        const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
            events.push(`invoke:${command}:${String(args?.expectedRevision ?? '')}`)
            await nativePending
            return {
                kind: 'updated',
                operationId: 'operation-exclusive',
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

        const first = facade.syncRegistered('device-first')
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledOnce())

        await expect(facade.syncRegistered('device-second'))
            .rejects.toThrow('A peer sync target is active')

        releaseNative()
        await expect(first).resolves.toMatchObject({ kind: 'updated', revision: 8 })
        expect(invoke).toHaveBeenCalledOnce()
        expect(events).toEqual([
            'flush:peer-bidirectional-sync',
            'capture:peer-bidirectional-sync',
            'acquire:7:11',
            'invoke:peer_bidirectional_sync_registered:7',
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

    it('returns the recovered durable local commit after a lost registered resolve response', async () => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_resolve_registered') throw new Error('native response lost')
            if (command === 'peer_bidirectional_status') {
                return {
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-registered-loss',
                        committedRevision: 8,
                    },
                }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop', runtime: runtime(events),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolveRegistered('source', 'operation-registered-loss', 'local'))
            .resolves.toMatchObject({
                kind: 'resumeRequired', operationId: 'operation-registered-loss', committedRevision: 8,
            })
    })

    it.each([
        {
            phase: 'localCommitted' as const,
            operation: {
                phase: 'localCommitted' as const,
                operationId: 'operation-registered-response-loss',
                committedRevision: 8,
            },
            expected: {
                kind: 'resumeRequired' as const,
                operationId: 'operation-registered-response-loss',
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
                    operationId: 'operation-registered-response-loss',
                    revision: 8,
                    remoteRevision: 9,
                    transferredObjects: 1,
                    transferredBytes: 12,
                    backups: [],
                },
            },
            expected: {
                kind: 'updated' as const,
                operationId: 'operation-registered-response-loss',
                revision: 8,
                remoteRevision: 9,
                transferredObjects: 1,
                transferredBytes: 12,
                backups: [],
            },
        },
    ])('returns exact registered resolve progress after a lost $phase response', async ({ operation, expected }) => {
        const events: string[] = []
        const invoke = vi.fn(async (command: string) => {
            events.push(`invoke:${command}`)
            if (command === 'peer_bidirectional_resolve_registered') {
                throw new Error('registered response lost')
            }
            if (command === 'peer_bidirectional_status') {
                return { operation }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolveRegistered(
            'device-registered-response-loss',
            'operation-registered-response-loss',
            'local',
        )).resolves.toEqual(expected)
        expect(events).toContain('refresh:8')
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_resolve_registered'))
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
                    operationId: 'operation-registered-rejected',
                    conflicts: [{ key: 'r1:root', type: 'sameRecord' as const }],
                    localManifestHash: 'c'.repeat(64),
                    remoteManifestHash: 'd'.repeat(64),
                },
            },
        },
    ])('preserves registered resolve rejection for $name', async ({ operation }) => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'peer_bidirectional_resolve_registered') {
                throw new Error('registered resolve rejected')
            }
            if (command === 'peer_bidirectional_status') {
                return { operation }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime([]),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolveRegistered(
            'device-registered-rejected',
            'operation-registered-rejected',
            'remote',
        )).rejects.toThrow('registered resolve rejected')
        expect(invoke.mock.calls.filter(([command]) => command === 'peer_bidirectional_resolve_registered'))
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
                if (command === 'peer_bidirectional_sync_registered') {
                    throw new Error('sync response lost')
                }
                if (command === 'peer_bidirectional_status') {
                    return {
                        operation,
                    }
                }
            })
            const facade = createPeerBidirectionalFacade({
                platform: 'desktop',
                runtime: runtime(events),
                invoke: invoke as unknown as PeerBidirectionalInvoke,
            })

            await expect(facade.syncRegistered('device-normalized')).rejects.toThrow('sync response lost')
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
                if (command === 'peer_bidirectional_sync_registered') {
                    throw new Error('sync failed before commit')
                }
                if (command === 'peer_bidirectional_status') {
                    return {
                        operation: { phase, operationId: `operation-${phase}` },
                    }
                }
            })
            const facade = createPeerBidirectionalFacade({
                platform: 'desktop',
                runtime: runtime(events),
                invoke: invoke as unknown as PeerBidirectionalInvoke,
            })

            await expect(facade.syncRegistered('device-precommit')).rejects.toThrow('sync failed before commit')

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
            if (command === 'peer_bidirectional_resolve_registered') throw new Error('backup creation failed')
            if (command === 'peer_bidirectional_status') {
                return { operation: conflict }
            }
        })
        const facade = createPeerBidirectionalFacade({
            platform: 'desktop',
            runtime: runtime(events),
            invoke: invoke as unknown as PeerBidirectionalInvoke,
        })

        await expect(facade.resolveRegistered('device-unchanged', 'operation-unchanged-conflict', 'remote'))
            .rejects.toThrow('backup creation failed')
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

        await expect(facade.syncRegistered('device-conflict')).resolves.toMatchObject({
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

        await expect(facade.syncRegistered('device-stale')).resolves.toMatchObject({ kind: 'stale' })
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
                async acquireDestructiveReplacementFence(token) {
                    const fence = await base.acquireDestructiveReplacementFence(token)
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

        await expect(facade.syncRegistered('device-retry')).rejects.toThrow('renderer refresh failed')
        await expect(facade.syncRegistered('device-retry')).resolves.toMatchObject({ kind: 'noChanges' })
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

        await facade.resolveRegistered('device-4', 'operation-4', 'local')
        expect(events).toContain('invoke:peer_bidirectional_resolve_registered:operation-4:local')
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
                async acquireDestructiveReplacementFence(token) {
                    const fence = await base.acquireDestructiveReplacementFence(token)
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

        await expect(facade.resolveRegistered('device-refresh', 'operation-refresh', 'local'))
            .rejects.toBeInstanceOf(PeerBidirectionalRefreshError)
        await expect(facade.resolveRegistered('device-refresh', 'operation-refresh', 'remote'))
            .rejects.toThrow('different peer sync retry')
        await expect(facade.resolveRegistered('device-refresh', 'operation-refresh', 'local'))
            .resolves.toMatchObject({ kind: 'updated' })
        expect(invoke).toHaveBeenCalledTimes(1)
        expect(events.filter((event) => event === 'release')).toHaveLength(1)
    })
})
