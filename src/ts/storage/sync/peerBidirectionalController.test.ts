import { describe, expect, it, vi } from 'vitest'

import { createPeerBidirectionalController } from './peerBidirectionalController'
import type {
    PeerBidirectionalCompletedResult,
    PeerBidirectionalFacade,
    PeerBidirectionalStatus,
    PeerBidirectionalSyncResult,
} from './peerBidirectional'
import { PeerBidirectionalRefreshError } from './peerBidirectional'

function idleStatus(): PeerBidirectionalStatus {
    return {
        source: { phase: 'idle', devices: [] },
        operation: undefined,
    }
}

function facade(overrides: Partial<PeerBidirectionalFacade> = {}): PeerBidirectionalFacade {
    return {
        capabilities: async () => ({
            desktop: true,
            sourceReady: true,
            atomicActivationReady: true,
            authenticatedTransportReady: true,
            losslessBackupReady: true,
            durableStateReady: true,
            productionEnabled: true,
        }),
        prepare: async () => ({ phase: 'prepared', sessionId: 'session-1', devices: [] }),
        start: async () => ({ phase: 'running', sessionId: 'session-1', pairingUri: 'pairing', devices: [] }),
        startQuickTunnel: async () => ({ phase: 'running', sessionId: 'session-1', pairingUri: 'quick', devices: [] }),
        startNamedTunnel: async () => ({ phase: 'running', sessionId: 'session-1', pairingUri: 'named', devices: [] }),
        status: async () => idleStatus(),
        stop: async () => undefined,
        revoke: async () => undefined,
        sync: async () => ({
            kind: 'noChanges',
            operationId: 'operation-1',
            revision: 1,
            remoteRevision: 1,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }),
        syncRegistered: async () => ({
            kind: 'noChanges', operationId: 'operation-1', revision: 1, remoteRevision: 1,
            transferredObjects: 0, transferredBytes: 0, backups: [],
        }),
        resolve: async () => ({
            kind: 'noChanges',
            operationId: 'operation-1',
            revision: 1,
            remoteRevision: 1,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }),
        resolveRegistered: async () => ({
            kind: 'noChanges', operationId: 'operation-1', revision: 1, remoteRevision: 1,
            transferredObjects: 0, transferredBytes: 0, backups: [],
        }),
        resume: async () => ({
            kind: 'noChanges',
            operationId: 'operation-1',
            revision: 1,
            remoteRevision: 1,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }),
        acknowledge: async () => undefined,
        ...overrides,
    }
}

describe('peer bidirectional controller', () => {
    it('initializes durable target state without starting the legacy source poller', async () => {
        vi.useFakeTimers()
        const status = vi.fn(async () => ({
            source: { phase: 'running' as const, sessionId: 'legacy-source', devices: [] },
            operation: {
                phase: 'targetPrepared' as const,
                operationId: 'operation-target',
            },
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({ status }),
            sourcePollMilliseconds: 10,
        })

        await controller.initializeTarget()
        await vi.advanceTimersByTimeAsync(20)

        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'targetPrepared',
            operationId: 'operation-target',
        })
        expect(status).toHaveBeenCalledTimes(1)
        vi.useRealTimers()
    })

    it('owns Quick and Named source tunnel starts through the shared source lifecycle', async () => {
        const startQuickTunnel = vi.fn(async () => ({
            phase: 'running' as const,
            sessionId: 'session-source',
            pairingUri: 'quick-link',
            devices: [],
        }))
        const startNamedTunnel = vi.fn(async () => ({
            phase: 'running' as const,
            sessionId: 'session-source',
            pairingUri: 'named-link',
            devices: [],
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({ startQuickTunnel, startNamedTunnel }),
        })
        await controller.initialize()

        await controller.startQuickTunnel('session-source')
        await controller.stop('session-source')
        await controller.startNamedTunnel('session-source', 'secret', 'https://sync.example.com')

        expect(startQuickTunnel).toHaveBeenCalledWith('session-source')
        expect(startNamedTunnel).toHaveBeenCalledWith(
            'session-source',
            'secret',
            'https://sync.example.com',
        )
    })
    it('retains one source action and its busy state for remounted subscribers', async () => {
        let finishStart!: (status: PeerBidirectionalStatus['source']) => void
        const start = vi.fn(() => new Promise<PeerBidirectionalStatus['source']>((resolve) => {
            finishStart = resolve
        }))
        const stop = vi.fn(async () => undefined)
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'prepared', sessionId: 'session-source', devices: [] },
                }),
                start,
                stop,
            }),
        })
        await controller.initialize()

        const first = controller.start('session-source')
        const remountedSnapshots: PeerBidirectionalStatus['source'][] = []
        const unsubscribe = controller.subscribe((snapshot) => {
            if (snapshot.sourceBusy) remountedSnapshots.push(snapshot.sourceStatus)
        })

        expect(controller.snapshot().sourceBusy).toBe(true)
        expect(controller.start('session-source')).toBe(first)
        await expect(controller.stop('session-source')).rejects.toThrow(
            'different peer sync source action is already running',
        )
        expect(start).toHaveBeenCalledTimes(1)
        expect(stop).not.toHaveBeenCalled()
        expect(remountedSnapshots).toHaveLength(1)

        finishStart({
            phase: 'running',
            sessionId: 'session-source',
            pairingUri: 'pairing',
            devices: [],
        })
        await first
        expect(controller.snapshot().sourceBusy).toBe(false)
        unsubscribe()
    })

    it('projects and resumes a durable target-prepared operation without a fake result', async () => {
        const completed: PeerBidirectionalCompletedResult = {
            kind: 'noChanges',
            operationId: 'operation-target-prepared',
            revision: 8,
            remoteRevision: 8,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }
        const resume = vi.fn(async () => completed)
        const controller = createPeerBidirectionalController({
            facade: facade({
                resume,
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'targetPrepared',
                        operationId: 'operation-target-prepared',
                    },
                } as unknown as PeerBidirectionalStatus),
            }),
        })
        await controller.initialize()

        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'targetPrepared',
            operationId: 'operation-target-prepared',
            operationResult: undefined,
            operationRetained: true,
        })
        await expect(controller.resume()).resolves.toEqual(completed)
        expect(resume).toHaveBeenCalledWith('operation-target-prepared')
    })

    it.each(['targetPrepared', 'awaitingConflict'] as const)(
        'keeps retained %s state when fresh-link recovery fails',
        async (retainedPhase) => {
            const conflict: PeerBidirectionalSyncResult = {
                kind: 'conflict',
                operationId: 'operation-reconnect',
                conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                localManifestHash: 'a'.repeat(64),
                remoteManifestHash: 'b'.repeat(64),
            }
            const operation = retainedPhase === 'targetPrepared'
                ? {
                      phase: 'targetPrepared' as const,
                      operationId: 'operation-reconnect',
                  }
                : { phase: 'awaitingConflict' as const, result: conflict }
            const sync = vi.fn(async () => { throw new Error('fresh peer unavailable') })
            const controller = createPeerBidirectionalController({
                facade: facade({
                    sync,
                    status: async () => ({
                        source: { phase: 'idle', devices: [] },
                        operation,
                    } as unknown as PeerBidirectionalStatus),
                }),
            })
            await controller.initialize()

            await expect(controller.sync('pairing-fresh')).rejects.toThrow('fresh peer unavailable')

            expect(sync).toHaveBeenCalledWith('pairing-fresh')
            expect(controller.snapshot()).toMatchObject({
                operationPhase: retainedPhase,
                operationId: 'operation-reconnect',
                operationResult: retainedPhase === 'awaitingConflict' ? conflict : undefined,
                operationRetained: true,
                operationError: 'fresh peer unavailable',
            })
        },
    )

    it('keeps target-prepared state when resume fails', async () => {
        const resume = vi.fn(async () => { throw new Error('resume peer unavailable') })
        const controller = createPeerBidirectionalController({
            facade: facade({
                resume,
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'targetPrepared',
                        operationId: 'operation-target-prepared',
                    },
                } as unknown as PeerBidirectionalStatus),
            }),
        })
        await controller.initialize()

        await expect(controller.resume()).rejects.toThrow('resume peer unavailable')

        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'targetPrepared',
            operationId: 'operation-target-prepared',
            operationResult: undefined,
            operationRetained: true,
            operationError: 'resume peer unavailable',
        })
    })

    it('refreshes status after clearing a target-prepared operation', async () => {
        const acknowledge = vi.fn(async () => undefined)
        let acknowledged = false
        acknowledge.mockImplementation(async () => { acknowledged = true })
        const status = vi.fn(async () => acknowledged
            ? idleStatus()
            : ({
                  source: { phase: 'idle', devices: [] },
                  operation: {
                      phase: 'targetPrepared',
                      operationId: 'operation-target-prepared',
                  },
              } as unknown as PeerBidirectionalStatus))
        const controller = createPeerBidirectionalController({
            facade: facade({ acknowledge, status }),
        })
        await controller.initialize()

        await controller.abandon()

        expect(acknowledge).toHaveBeenCalledWith('operation-target-prepared')
        expect(status).toHaveBeenCalledTimes(2)
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'idle',
            operationId: undefined,
            operationRetained: false,
        })
    })

    it('abandons an awaiting conflict explicitly', async () => {
        const conflict: PeerBidirectionalSyncResult = {
            kind: 'conflict',
            operationId: 'operation-conflict-abandon',
            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
            localManifestHash: 'a'.repeat(64),
            remoteManifestHash: 'b'.repeat(64),
        }
        const acknowledge = vi.fn(async () => undefined)
        const controller = createPeerBidirectionalController({
            facade: facade({
                acknowledge,
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'awaitingConflict', result: conflict },
                }),
            }),
        })
        await controller.initialize()

        await controller.abandon()

        expect(acknowledge).toHaveBeenCalledWith('operation-conflict-abandon')
        expect(controller.snapshot()).toMatchObject({ operationPhase: 'idle', operationRetained: false })
    })

    it('preserves an awaiting conflict when abandon fails', async () => {
        const conflict: PeerBidirectionalSyncResult = {
            kind: 'conflict',
            operationId: 'operation-conflict-retained',
            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
            localManifestHash: 'a'.repeat(64),
            remoteManifestHash: 'b'.repeat(64),
        }
        const controller = createPeerBidirectionalController({
            facade: facade({
                acknowledge: async () => { throw new Error('clear failed') },
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'awaitingConflict', result: conflict },
                }),
            }),
        })
        await controller.initialize()

        await expect(controller.abandon()).rejects.toThrow('clear failed')

        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'awaitingConflict',
            operationId: 'operation-conflict-retained',
            operationResult: conflict,
            operationRetained: true,
            operationError: 'clear failed',
        })
    })

    it('projects a durable source-prepared operation after restart without a resume result', async () => {
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'stopped', devices: [] },
                    operation: {
                        phase: 'sourcePrepared',
                        operationId: 'operation-source-prepared',
                    },
                } as unknown as PeerBidirectionalStatus),
            }),
        })

        await controller.initialize()

        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'sourcePrepared',
            operationId: 'operation-source-prepared',
            operationResult: undefined,
            operationRetained: true,
        })
    })

    it('rehosts a source while a source-prepared operation is retained', async () => {
        const prepare = vi.fn(facade().prepare)
        const start = vi.fn(facade().start)
        const controller = createPeerBidirectionalController({
            facade: facade({
                prepare,
                start,
                status: async () => ({
                    source: { phase: 'stopped', devices: [] },
                    operation: {
                        phase: 'sourcePrepared',
                        operationId: 'operation-source-prepared',
                    },
                } as unknown as PeerBidirectionalStatus),
            }),
        })
        await controller.initialize()

        await controller.prepare()
        await controller.start('session-1')

        expect(prepare).toHaveBeenCalledTimes(1)
        expect(start).toHaveBeenCalledWith('session-1')
        expect(controller.snapshot()).toMatchObject({
            sourceStatus: { phase: 'running' },
            operationPhase: 'sourcePrepared',
            operationId: 'operation-source-prepared',
            operationResult: undefined,
            operationRetained: true,
        })
    })

    it('keeps target sync blocked while a source-prepared operation is retained', async () => {
        const sync = vi.fn(facade().sync)
        const controller = createPeerBidirectionalController({
            facade: facade({
                sync,
                status: async () => ({
                    source: { phase: 'stopped', devices: [] },
                    operation: {
                        phase: 'sourcePrepared',
                        operationId: 'operation-source-prepared',
                    },
                } as unknown as PeerBidirectionalStatus),
            }),
        })
        await controller.initialize()

        await expect(controller.sync('pairing-fresh')).rejects.toThrow('retained peer sync operation')
        expect(sync).not.toHaveBeenCalled()
    })

    it('abandons a stopped source-prepared operation and preserves a completed receipt', async () => {
        const completed: PeerBidirectionalCompletedResult = {
            kind: 'noChanges',
            operationId: 'operation-source-prepared',
            revision: 9,
            remoteRevision: 9,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [{ packageId: 'backup-source-prepared', side: 'local', path: 'source.risulossless' }],
        }
        const acknowledge = vi.fn(async () => undefined)
        let acknowledged = false
        acknowledge.mockImplementation(async () => { acknowledged = true })
        const controller = createPeerBidirectionalController({
            facade: facade({
                acknowledge,
                status: async () => acknowledged
                    ? {
                          source: { phase: 'stopped', devices: [] },
                          operation: { phase: 'completed', result: completed },
                      }
                    : ({
                          source: { phase: 'stopped', devices: [] },
                          operation: {
                              phase: 'sourcePrepared',
                              operationId: 'operation-source-prepared',
                          },
                      } as unknown as PeerBidirectionalStatus),
            }),
        })
        await controller.initialize()

        await controller.abandon()

        expect(acknowledge).toHaveBeenCalledWith('operation-source-prepared')
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'completed',
            operationId: 'operation-source-prepared',
            operationResult: completed,
            operationRetained: true,
        })
    })

    it.each(['prepared', 'running'] as const)(
        'does not abandon a source-prepared operation while the source is %s',
        async (sourcePhase) => {
            const acknowledge = vi.fn(async () => undefined)
            const controller = createPeerBidirectionalController({
                facade: facade({
                    acknowledge,
                    status: async () => ({
                        source: { phase: sourcePhase, sessionId: 'session-source', devices: [] },
                        operation: {
                            phase: 'sourcePrepared',
                            operationId: 'operation-source-prepared',
                        },
                    } as unknown as PeerBidirectionalStatus),
                }),
            })
            await controller.initialize()

            await controller.abandon()

            expect(acknowledge).not.toHaveBeenCalled()
            expect(controller.snapshot()).toMatchObject({
                operationPhase: 'sourcePrepared',
                operationId: 'operation-source-prepared',
                operationResult: undefined,
                operationRetained: true,
            })
        },
    )

    it('projects source-side completion from polling and clears a retried refresh error', async () => {
        vi.useFakeTimers()
        try {
            const completed: PeerBidirectionalSyncResult = {
                kind: 'updated',
                operationId: 'operation-source-poll',
                revision: 8,
                remoteRevision: 9,
                transferredObjects: 1,
                transferredBytes: 14,
                backups: [{ packageId: 'backup-source-poll', side: 'local', path: 'source.risulossless' }],
            }
            const completedStatus: PeerBidirectionalStatus = {
                source: { phase: 'running', sessionId: 'session-source', devices: [] },
                operation: { phase: 'completed', result: completed },
            }
            let statusCalls = 0
            const controller = createPeerBidirectionalController({
                sourcePollMilliseconds: 10,
                facade: facade({
                    status: async () => {
                        statusCalls += 1
                        if (statusCalls === 1) {
                            return { source: { phase: 'running', sessionId: 'session-source', devices: [] } }
                        }
                        if (statusCalls === 2) {
                            throw new PeerBidirectionalRefreshError(
                                completed,
                                new Error('source refresh failed'),
                                completedStatus,
                            )
                        }
                        return completedStatus
                    },
                }),
            })

            await controller.initialize()
            await vi.advanceTimersByTimeAsync(10)
            expect(controller.snapshot()).toMatchObject({
                operationPhase: 'completed',
                operationResult: completed,
                operationError: 'source refresh failed',
            })

            await vi.advanceTimersByTimeAsync(10)
            expect(controller.snapshot()).toMatchObject({
                operationPhase: 'completed',
                operationResult: completed,
                operationError: '',
            })
        } finally {
            vi.useRealTimers()
        }
    })

    it('preserves a later acknowledge error after stopping a source with a refresh error', async () => {
        vi.useFakeTimers()
        try {
            const completed: PeerBidirectionalSyncResult = {
                kind: 'noChanges',
                operationId: 'operation-acknowledge-error',
                revision: 8,
                remoteRevision: 8,
                transferredObjects: 0,
                transferredBytes: 0,
                backups: [],
            }
            const completedStatus: PeerBidirectionalStatus = {
                source: { phase: 'running', sessionId: 'session-source', devices: [] },
                operation: { phase: 'completed', result: completed },
            }
            const stoppedStatus: PeerBidirectionalStatus = {
                source: { phase: 'stopped', sessionId: 'session-source', devices: [] },
                operation: { phase: 'completed', result: completed },
            }
            let statusCalls = 0
            const controller = createPeerBidirectionalController({
                sourcePollMilliseconds: 10,
                facade: facade({
                    status: async () => {
                        statusCalls += 1
                        if (statusCalls === 2) {
                            throw new PeerBidirectionalRefreshError(
                                completed,
                                new Error('source refresh failed'),
                                completedStatus,
                            )
                        }
                        return statusCalls > 2 ? stoppedStatus : completedStatus
                    },
                    acknowledge: async () => { throw new Error('acknowledge failed') },
                }),
            })

            await controller.initialize()
            await vi.advanceTimersByTimeAsync(10)
            expect(controller.snapshot().operationError).toBe('source refresh failed')
            await controller.stop('session-source')
            await expect(controller.acknowledge()).rejects.toThrow('acknowledge failed')
            await vi.advanceTimersByTimeAsync(10)
            expect(controller.snapshot()).toMatchObject({
                operationPhase: 'completed',
                operationError: 'acknowledge failed',
            })
        } finally {
            vi.useRealTimers()
        }
    })

    it('ignores a pre-stop poll that resolves after the stopped status projection', async () => {
        vi.useFakeTimers()
        try {
            const resultForRevision = (revision: number): PeerBidirectionalCompletedResult => ({
                kind: 'updated',
                operationId: 'operation-stop-poll-race',
                revision,
                remoteRevision: 9,
                transferredObjects: 1,
                transferredBytes: 12,
                backups: [],
            })
            let finishPoll!: (status: PeerBidirectionalStatus) => void
            let statusCalls = 0
            const controller = createPeerBidirectionalController({
                sourcePollMilliseconds: 10,
                facade: facade({
                    status: () => {
                        statusCalls += 1
                        if (statusCalls === 1) {
                            return Promise.resolve({
                                source: { phase: 'running', sessionId: 'session-source', devices: [] },
                            })
                        }
                        if (statusCalls === 2) {
                            return new Promise((resolve) => {
                                finishPoll = resolve
                            })
                        }
                        return Promise.resolve({
                            source: { phase: 'stopped', sessionId: 'session-source', devices: [] },
                            operation: { phase: 'completed', result: resultForRevision(9) },
                        })
                    },
                }),
            })

            await controller.initialize()
            await vi.advanceTimersByTimeAsync(10)
            expect(statusCalls).toBe(2)
            await controller.stop('session-source')
            finishPoll({
                source: { phase: 'running', sessionId: 'session-source', devices: [] },
                operation: { phase: 'completed', result: resultForRevision(8) },
            })
            await Promise.resolve()
            await Promise.resolve()
            expect(controller.snapshot()).toMatchObject({
                sourceStatus: { phase: 'stopped' },
                operationPhase: 'completed',
                operationResult: { revision: 9 },
            })
        } finally {
            vi.useRealTimers()
        }
    })

    it('adopts an awaiting-choice operation from durable native status after restart', async () => {
        const conflict: PeerBidirectionalSyncResult = {
            kind: 'conflict',
            operationId: 'operation-recovered',
            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
            localManifestHash: 'a'.repeat(64),
            remoteManifestHash: 'b'.repeat(64),
        }
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'awaitingConflict', result: conflict },
                }),
            }),
        })

        await controller.initialize()
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'awaitingConflict',
            operationResult: conflict,
        })
    })

    it('shares one promise for the same pairing and rejects a different concurrent pairing', async () => {
        let finish!: (value: PeerBidirectionalSyncResult) => void
        const sync = vi.fn(() => new Promise<PeerBidirectionalSyncResult>((resolve) => {
            finish = resolve
        }))
        const controller = createPeerBidirectionalController({ facade: facade({ sync }) })
        const first = controller.sync('pairing-a')
        const same = controller.sync('pairing-a')
        await expect(controller.sync('pairing-b')).rejects.toThrow('different pairing')
        expect(first).toBe(same)
        finish({
            kind: 'noChanges',
            operationId: 'operation-shared',
            revision: 1,
            remoteRevision: 1,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        })
        await first
        expect(sync).toHaveBeenCalledTimes(1)
    })

    it('retains explicit conflicts until the user chooses one side', async () => {
        const resolve = vi.fn(async () => ({
            kind: 'updated' as const,
            operationId: 'operation-conflict',
            revision: 4,
            remoteRevision: 3,
            transferredObjects: 1,
            transferredBytes: 9,
            backups: [{ packageId: 'backup-1', side: 'remote' as const, path: 'backup.risulossless' }],
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({
                sync: async () => ({
                    kind: 'conflict',
                    operationId: 'operation-conflict',
                    conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                    localManifestHash: 'a'.repeat(64),
                    remoteManifestHash: 'b'.repeat(64),
                }),
                resolve,
            }),
        })

        await controller.sync('pairing')
        expect(controller.snapshot().operationPhase).toBe('awaitingConflict')
        await controller.resolve('local')
        expect(resolve).toHaveBeenCalledWith('operation-conflict', 'local')
        expect(controller.snapshot()).toMatchObject({ operationPhase: 'completed' })
    })

    it('passes a fresh conflict link atomically with the selected winner', async () => {
        const resolve = vi.fn(async () => ({
            kind: 'noChanges' as const,
            operationId: 'operation-public-conflict',
            revision: 4,
            remoteRevision: 4,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'awaitingConflict',
                        result: {
                            kind: 'conflict',
                            operationId: 'operation-public-conflict',
                            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                            localManifestHash: 'a'.repeat(64),
                            remoteManifestHash: 'b'.repeat(64),
                        },
                    },
                }),
                resolve,
            }),
        })
        await controller.initialize()

        await controller.resolve('remote', 'fresh-public-link')

        expect(resolve).toHaveBeenCalledWith(
            'operation-public-conflict',
            'remote',
            'fresh-public-link',
        )
    })

    it('resumes a native committed operation recovered after restart', async () => {
        const resume = vi.fn(async () => ({
            kind: 'noChanges' as const,
            operationId: 'operation-resume',
            revision: 6,
            remoteRevision: 8,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-resume',
                        committedRevision: 6,
                    },
                }),
                resume,
            }),
        })

        await controller.initialize()
        await controller.resume()
        expect(resume).toHaveBeenCalledWith('operation-resume')
        expect(controller.snapshot().operationPhase).toBe('completed')
    })

    it('coalesces only identical retained resolve and resume operations', async () => {
        let finishResolve!: (value: PeerBidirectionalSyncResult) => void
        const resolve = vi.fn(() => new Promise<PeerBidirectionalSyncResult>((done) => {
            finishResolve = done
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'awaitingConflict',
                        result: {
                            kind: 'conflict',
                            operationId: 'operation-choice',
                            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                            localManifestHash: 'a'.repeat(64),
                            remoteManifestHash: 'b'.repeat(64),
                        },
                    },
                }),
                resolve,
            }),
        })
        await controller.initialize()

        const first = controller.resolve('local')
        const same = controller.resolve('local')
        await expect(controller.resolve('remote')).rejects.toThrow('different peer sync operation')
        expect(first).toBe(same)
        finishResolve({
            kind: 'noChanges',
            operationId: 'operation-choice',
            revision: 2,
            remoteRevision: 2,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        })
        await first
        expect(resolve).toHaveBeenCalledTimes(1)

        let finishResume!: (value: PeerBidirectionalSyncResult) => void
        const resume = vi.fn(() => new Promise<PeerBidirectionalSyncResult>((done) => {
            finishResume = done
        }))
        const resumed = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'localCommitted', operationId: 'operation-resume', committedRevision: 3 },
                }),
                resume,
            }),
        })
        await resumed.initialize()
        const resumeFirst = resumed.resume()
        expect(resumed.resume()).toBe(resumeFirst)
        finishResume({
            kind: 'noChanges',
            operationId: 'operation-resume',
            revision: 3,
            remoteRevision: 3,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        })
        await resumeFirst
        expect(resume).toHaveBeenCalledTimes(1)
    })

    it('keeps source and target errors in separate ownership domains', async () => {
        const controller = createPeerBidirectionalController({
            facade: facade({
                prepare: async () => { throw new Error('source failed') },
                sync: async () => { throw new Error('target failed') },
            }),
        })

        await expect(controller.sync('pairing')).rejects.toThrow('target failed')
        await expect(controller.prepare()).rejects.toThrow('source failed')
        expect(controller.snapshot()).toMatchObject({
            operationError: 'target failed',
            sourceError: 'source failed',
        })
    })

    it('keeps a retained native phase and backup result after a recoverable transport error', async () => {
        const completed: PeerBidirectionalSyncResult = {
            kind: 'updated',
            operationId: 'operation-retained-error',
            revision: 4,
            remoteRevision: 5,
            transferredObjects: 1,
            transferredBytes: 9,
            backups: [{ packageId: 'backup-retained', side: 'remote', path: 'remote.risulossless' }],
        }
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'completed', result: completed },
                }),
                acknowledge: async () => { throw new Error('transport unavailable') },
            }),
        })

        await controller.initialize()
        await expect(controller.acknowledge()).rejects.toThrow('transport unavailable')
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'completed',
            operationResult: completed,
            operationId: 'operation-retained-error',
            operationError: 'transport unavailable',
        })
    })

    it('preserves awaiting conflict and local committed phases when resolve or resume fails', async () => {
        const conflict: PeerBidirectionalSyncResult = {
            kind: 'conflict',
            operationId: 'operation-conflict-error',
            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
            localManifestHash: 'a'.repeat(64),
            remoteManifestHash: 'b'.repeat(64),
        }
        const conflicted = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'awaitingConflict', result: conflict },
                }),
                resolve: async () => { throw new Error('resolve transport failed') },
            }),
        })
        await conflicted.initialize()
        await expect(conflicted.resolve('local')).rejects.toThrow('resolve transport failed')
        expect(conflicted.snapshot()).toMatchObject({
            operationPhase: 'awaitingConflict',
            operationResult: conflict,
            operationError: 'resolve transport failed',
        })

        const committed = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-committed-error',
                        committedRevision: 7,
                    },
                }),
                resume: async () => { throw new Error('resume transport failed') },
            }),
        })
        await committed.initialize()
        await expect(committed.resume()).rejects.toThrow('resume transport failed')
        expect(committed.snapshot()).toMatchObject({
            operationPhase: 'localCommitted',
            operationId: 'operation-committed-error',
            operationError: 'resume transport failed',
        })
    })

    it('recovers the latest native phase after a sync, resolve, or resume response is lost', async () => {
        const conflict: PeerBidirectionalSyncResult = {
            kind: 'conflict',
            operationId: 'operation-response-loss',
            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
            localManifestHash: 'a'.repeat(64),
            remoteManifestHash: 'b'.repeat(64),
        }
        let syncStatusCalls = 0
        const synced = createPeerBidirectionalController({
            facade: facade({
                status: async () => syncStatusCalls++ === 0 ? idleStatus() : ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'awaitingConflict', result: conflict },
                }),
                sync: async () => { throw new Error('sync response lost') },
            }),
        })
        await synced.initialize()
        await expect(synced.sync('pairing')).rejects.toThrow('sync response lost')
        expect(synced.snapshot()).toMatchObject({
            operationPhase: 'awaitingConflict',
            operationResult: conflict,
            operationError: 'sync response lost',
        })

        let resolveStatusCalls = 0
        const resolved = createPeerBidirectionalController({
            facade: facade({
                status: async () => resolveStatusCalls++ === 0 ? ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'awaitingConflict', result: conflict },
                }) : ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-response-loss',
                        committedRevision: 6,
                    },
                }),
                resolve: async () => { throw new Error('resolve response lost') },
            }),
        })
        await resolved.initialize()
        await expect(resolved.resolve('local')).rejects.toThrow('resolve response lost')
        expect(resolved.snapshot()).toMatchObject({
            operationPhase: 'localCommitted',
            operationId: 'operation-response-loss',
            operationError: 'resolve response lost',
        })

        const completed: PeerBidirectionalSyncResult = {
            kind: 'updated',
            operationId: 'operation-response-loss',
            revision: 6,
            remoteRevision: 7,
            transferredObjects: 1,
            transferredBytes: 10,
            backups: [{ packageId: 'backup-response-loss', side: 'local', path: 'local.risulossless' }],
        }
        let resumeStatusCalls = 0
        const resumed = createPeerBidirectionalController({
            facade: facade({
                status: async () => resumeStatusCalls++ === 0 ? ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-response-loss',
                        committedRevision: 6,
                    },
                }) : ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'completed', result: completed },
                }),
                resume: async () => { throw new Error('resume response lost') },
            }),
        })
        await resumed.initialize()
        await expect(resumed.resume()).rejects.toThrow('resume response lost')
        expect(resumed.snapshot()).toMatchObject({
            operationPhase: 'completed',
            operationResult: completed,
            operationError: 'resume response lost',
        })
    })

    it.each(['sourcePrepared', 'targetPrepared'] as const)(
        'retains a proven-precommit %s phase after a failed mutation',
        async (phase) => {
            let statusCalls = 0
            const controller = createPeerBidirectionalController({
                facade: facade({
                    status: async () => statusCalls++ === 0
                        ? idleStatus()
                        : ({
                              source: { phase: 'idle', devices: [] },
                              operation: { phase, operationId: `operation-${phase}` },
                          } as PeerBidirectionalStatus),
                    sync: async () => { throw new Error('sync failed before commit') },
                }),
            })
            await controller.initialize()

            await expect(controller.sync('pairing')).rejects.toThrow('sync failed before commit')

            expect(controller.snapshot()).toMatchObject({
                operationPhase: phase,
                operationId: `operation-${phase}`,
                operationResult: undefined,
                operationRetained: true,
                operationError: 'sync failed before commit',
            })
        },
    )

    it('rejects target start while source is active and source start while target is retained', async () => {
        const sync = vi.fn(facade().sync)
        const sourceActive = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'running', sessionId: 'session-source', devices: [] },
                }),
                sync,
            }),
        })
        await sourceActive.initialize()
        await expect(sourceActive.sync('pairing')).rejects.toThrow('peer sync source is active')
        expect(sync).not.toHaveBeenCalled()

        const start = vi.fn(facade().start)
        const targetRetained = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'prepared', sessionId: 'session-source', devices: [] },
                    operation: { phase: 'localCommitted', operationId: 'operation-target', committedRevision: 4 },
                }),
                start,
            }),
        })
        await targetRetained.initialize()
        await expect(targetRetained.start('session-source')).rejects.toThrow('retained peer sync operation')
        expect(start).not.toHaveBeenCalled()
    })

    it('allows source stop while conflict, local commit, or completion is retained', async () => {
        for (const operation of [
            {
                phase: 'awaitingConflict' as const,
                result: {
                    kind: 'conflict' as const,
                    operationId: 'operation-conflict-stop',
                    conflicts: [{ key: 'r1:root', type: 'sameRecord' as const }],
                    localManifestHash: 'a'.repeat(64),
                    remoteManifestHash: 'b'.repeat(64),
                },
            },
            {
                phase: 'localCommitted' as const,
                operationId: 'operation-committed-stop',
                committedRevision: 4,
            },
            {
                phase: 'completed' as const,
                result: {
                    kind: 'noChanges' as const,
                    operationId: 'operation-completed-stop',
                    revision: 4,
                    remoteRevision: 4,
                    transferredObjects: 0,
                    transferredBytes: 0,
                    backups: [],
                },
            },
        ]) {
            const stop = vi.fn(async () => undefined)
            const controller = createPeerBidirectionalController({
                facade: facade({
                    status: async () => ({
                        source: { phase: 'running', sessionId: 'session-source', devices: [] },
                        operation,
                    }),
                    stop,
                }),
            })
            await controller.initialize()
            await expect(controller.stop('session-source')).resolves.toBeUndefined()
            expect(stop).toHaveBeenCalledWith('session-source')
        }
    })

    it('projects completion and backup receipts discovered while stopping the source', async () => {
        const completed: PeerBidirectionalSyncResult = {
            kind: 'updated',
            operationId: 'operation-stop-projection',
            revision: 8,
            remoteRevision: 9,
            transferredObjects: 1,
            transferredBytes: 12,
            backups: [{ packageId: 'backup-stop', side: 'local', path: 'stop.risulossless' }],
        }
        let statusCalls = 0
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => {
                    statusCalls += 1
                    if (statusCalls === 1) {
                        return { source: { phase: 'running', sessionId: 'session-source', devices: [] } }
                    }
                    return {
                        source: { phase: 'stopped', sessionId: 'session-source', devices: [] },
                        operation: { phase: 'completed', result: completed },
                    }
                },
            }),
        })

        await controller.initialize()
        await controller.stop('session-source')
        expect(controller.snapshot()).toMatchObject({
            sourceStatus: { phase: 'stopped' },
            operationPhase: 'completed',
            operationResult: completed,
            operationRetained: true,
        })
    })

    it('revokes a durable offline device with the empty session sentinel and refreshes status', async () => {
        const revoke = vi.fn(async () => undefined)
        let revoked = false
        const controller = createPeerBidirectionalController({
            facade: facade({
                revoke,
                status: async () => ({
                    source: {
                        phase: 'stopped',
                        devices: [{
                            deviceId: 'device-offline',
                            transferredBytes: 14,
                            lastSeenAt: 1,
                            revoked,
                        }],
                    },
                }),
            }),
        })
        await controller.initialize()
        revoked = true

        await controller.revoke('', 'device-offline')

        expect(revoke).toHaveBeenCalledWith('', 'device-offline')
        expect(controller.snapshot().sourceStatus.devices[0]).toMatchObject({
            deviceId: 'device-offline',
            revoked: true,
        })
    })

    it('projects source-unavailable as a recoverable retained operation', async () => {
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: { phase: 'localCommitted', operationId: 'operation-source', committedRevision: 7 },
                }),
                resume: async () => ({
                    kind: 'sourceUnavailable',
                    operationId: 'operation-source',
                    committedRevision: 7,
                }),
            }),
        })

        await controller.initialize()
        await controller.resume()
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'sourceUnavailable',
            operationId: 'operation-source',
        })
    })

    it('rejects source prepare but permits fresh-link recovery for an awaiting conflict', async () => {
        const prepare = vi.fn(async () => ({ phase: 'prepared' as const, sessionId: 'session', devices: [] }))
        const sync = vi.fn(async () => ({
            kind: 'noChanges' as const,
            operationId: 'operation-new',
            revision: 4,
            remoteRevision: 4,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'awaitingConflict',
                        result: {
                            kind: 'conflict',
                            operationId: 'operation-retained',
                            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                            localManifestHash: 'a'.repeat(64),
                            remoteManifestHash: 'b'.repeat(64),
                        },
                    },
                }),
                prepare,
                sync,
            }),
        })
        await controller.initialize()

        await expect(controller.prepare()).rejects.toThrow('retained peer sync operation')
        await expect(controller.sync('pairing-new')).resolves.toMatchObject({ kind: 'noChanges' })
        expect(prepare).not.toHaveBeenCalled()
        expect(sync).toHaveBeenCalledWith('pairing-new')
    })

    it('keeps a committed renderer-refresh failure recoverable through resume', async () => {
        const completed: PeerBidirectionalSyncResult = {
            kind: 'updated',
            operationId: 'operation-refresh',
            revision: 9,
            remoteRevision: 8,
            transferredObjects: 1,
            transferredBytes: 12,
            backups: [],
        }
        let failRefresh = true
        const resolve = vi.fn(async () => {
            if (failRefresh) {
                failRefresh = false
                throw new PeerBidirectionalRefreshError(completed, new Error('renderer refresh failed'))
            }
            return completed
        })
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
                    source: { phase: 'idle', devices: [] },
                    operation: {
                        phase: 'awaitingConflict',
                        result: {
                            kind: 'conflict',
                            operationId: 'operation-refresh',
                            conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                            localManifestHash: 'a'.repeat(64),
                            remoteManifestHash: 'b'.repeat(64),
                        },
                    },
                }),
                resolve,
            }),
        })
        await controller.initialize()

        await expect(controller.resolve('local')).rejects.toThrow('renderer refresh failed')
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'refreshPending',
            operationResult: completed,
            operationRetained: true,
        })
        await expect(controller.resume()).resolves.toBe(completed)
        expect(resolve).toHaveBeenCalledTimes(2)
        expect(resolve).toHaveBeenNthCalledWith(1, 'operation-refresh', 'local')
        expect(resolve).toHaveBeenNthCalledWith(2, 'operation-refresh', 'local')
        expect(controller.snapshot().operationPhase).toBe('completed')
    })

    it('acknowledges and clears a retained terminal result', async () => {
        const acknowledge = vi.fn(async () => undefined)
        const controller = createPeerBidirectionalController({ facade: facade({ acknowledge }) })
        await controller.sync('pairing')
        expect(controller.snapshot().operationPhase).toBe('completed')

        await controller.acknowledge()

        expect(acknowledge).toHaveBeenCalledWith('operation-1')
        expect(controller.snapshot()).toMatchObject({ operationPhase: 'idle', operationId: undefined })
    })

    it.each(['prepared', 'running'] as const)(
        'does not acknowledge a completed result while the source is %s',
        async (sourcePhase) => {
            const acknowledge = vi.fn(async () => undefined)
            const controller = createPeerBidirectionalController({
                facade: facade({
                    acknowledge,
                    status: async () => ({
                        source: { phase: sourcePhase, sessionId: 'session-source', devices: [] },
                        operation: {
                            phase: 'completed',
                            result: {
                                kind: 'noChanges',
                                operationId: 'operation-source-active',
                                revision: 8,
                                remoteRevision: 8,
                                transferredObjects: 0,
                                transferredBytes: 0,
                                backups: [],
                            },
                        },
                    }),
                }),
            })
            await controller.initialize()

            await controller.acknowledge()

            expect(acknowledge).not.toHaveBeenCalled()
            expect(controller.snapshot()).toMatchObject({
                operationPhase: 'completed',
                operationId: 'operation-source-active',
                operationRetained: true,
            })
        },
    )

    it.each(['localCommitted', 'sourceUnavailable'] as const)(
        'abandons a retained %s operation and permits a new sync',
        async (retainedPhase) => {
            const acknowledge = vi.fn(async () => undefined)
            const sync = vi.fn(async () => ({
                kind: 'noChanges' as const,
                operationId: 'operation-next',
                revision: 8,
                remoteRevision: 8,
                transferredObjects: 0,
                transferredBytes: 0,
                backups: [],
            }))
            const controller = createPeerBidirectionalController({
                facade: facade({
                    acknowledge,
                    sync,
                    status: async () => ({
                        source: { phase: 'idle', devices: [] },
                        operation: {
                            phase: 'localCommitted',
                            operationId: 'operation-abandon',
                            committedRevision: 7,
                        },
                    }),
                    resume: async () => ({
                        kind: 'sourceUnavailable',
                        operationId: 'operation-abandon',
                        committedRevision: 7,
                    }),
                }),
            })
            await controller.initialize()
            if (retainedPhase === 'sourceUnavailable') await controller.resume()

            await controller.abandon()

            expect(acknowledge).toHaveBeenCalledWith('operation-abandon')
            expect(controller.snapshot()).toMatchObject({
                operationPhase: 'idle',
                operationId: undefined,
                operationRetained: false,
            })
            await controller.sync('pairing-next')
            expect(sync).toHaveBeenCalledWith('pairing-next')
        },
    )

    it.each(['localCommitted', 'sourceUnavailable'] as const)(
        'uses a fresh pairing to recover a retained %s operation',
        async (retainedPhase) => {
            const sync = vi.fn(facade().sync)
            const controller = createPeerBidirectionalController({
                facade: facade({
                    sync,
                    status: async () => ({
                        source: { phase: 'idle', devices: [] },
                        operation: {
                            phase: 'localCommitted',
                            operationId: 'operation-rebind',
                            committedRevision: 7,
                        },
                    }),
                    resume: async () => ({
                        kind: 'sourceUnavailable',
                        operationId: 'operation-rebind',
                        committedRevision: 7,
                    }),
                }),
            })
            await controller.initialize()
            if (retainedPhase === 'sourceUnavailable') await controller.resume()

            await expect(controller.sync('pairing-fresh')).resolves.toMatchObject({ kind: 'noChanges' })

            expect(sync).toHaveBeenCalledWith('pairing-fresh')
            expect(controller.snapshot().operationPhase).toBe('completed')
        },
    )

    it('prepares and starts a new source while a completed operation is retained', async () => {
        const completed: PeerBidirectionalSyncResult = {
            kind: 'noChanges',
            operationId: 'operation-source-rehost',
            revision: 7,
            remoteRevision: 7,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }
        const prepare = vi.fn(async () => ({
            phase: 'prepared' as const,
            sessionId: 'session-rehost',
            devices: [],
        }))
        const start = vi.fn(async () => ({
            phase: 'running' as const,
            sessionId: 'session-rehost',
            devices: [],
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({
                prepare,
                start,
                status: async () => ({
                    source: { phase: 'stopped', devices: [] },
                    operation: { phase: 'completed', result: completed },
                }),
            }),
        })
        await controller.initialize()

        await controller.prepare()
        await controller.start('session-rehost')

        expect(prepare).toHaveBeenCalledTimes(1)
        expect(start).toHaveBeenCalledWith('session-rehost')
        expect(controller.snapshot()).toMatchObject({
            sourceStatus: { phase: 'running' },
            operationPhase: 'completed',
            operationResult: completed,
            operationRetained: true,
        })
    })

    it('retains completed operation state when native rejects source rehost', async () => {
        const completed: PeerBidirectionalSyncResult = {
            kind: 'noChanges',
            operationId: 'operation-source-rehost-rejected',
            revision: 7,
            remoteRevision: 7,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
        }
        const prepare = vi.fn(async () => { throw new Error('completed operation belongs to target') })
        const controller = createPeerBidirectionalController({
            facade: facade({
                prepare,
                status: async () => ({
                    source: { phase: 'stopped', devices: [] },
                    operation: { phase: 'completed', result: completed },
                }),
            }),
        })
        await controller.initialize()

        await expect(controller.prepare()).rejects.toThrow('completed operation belongs to target')

        expect(prepare).toHaveBeenCalledTimes(1)
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'completed',
            operationResult: completed,
            operationRetained: true,
            sourceError: 'completed operation belongs to target',
        })
    })

    it.each(['awaitingConflict', 'targetPrepared', 'localCommitted', 'sourceUnavailable', 'refreshPending'] as const)(
        'keeps source prepare and start blocked while %s is retained',
        async (retainedPhase) => {
            const conflict: PeerBidirectionalSyncResult = {
                kind: 'conflict',
                operationId: 'operation-source-blocked',
                conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                localManifestHash: 'a'.repeat(64),
                remoteManifestHash: 'b'.repeat(64),
            }
            const completed: PeerBidirectionalSyncResult = {
                kind: 'noChanges',
                operationId: 'operation-source-blocked',
                revision: 7,
                remoteRevision: 7,
                transferredObjects: 0,
                transferredBytes: 0,
                backups: [],
            }
            const prepare = vi.fn(facade().prepare)
            const start = vi.fn(facade().start)
            const controller = createPeerBidirectionalController({
                facade: facade({
                    prepare,
                    start,
                    status: async () => ({
                        source: { phase: 'stopped', devices: [] },
                        operation: retainedPhase === 'awaitingConflict'
                            ? { phase: 'awaitingConflict', result: conflict }
                            : retainedPhase === 'targetPrepared'
                                ? {
                                      phase: 'targetPrepared',
                                      operationId: 'operation-source-blocked',
                                  }
                            : retainedPhase === 'refreshPending'
                                ? undefined
                                : {
                                      phase: 'localCommitted',
                                      operationId: 'operation-source-blocked',
                                      committedRevision: 7,
                                  },
                    }),
                    resume: async () => ({
                        kind: 'sourceUnavailable',
                        operationId: 'operation-source-blocked',
                        committedRevision: 7,
                    }),
                    sync: async () => {
                        throw new PeerBidirectionalRefreshError(completed, new Error('refresh failed'))
                    },
                }),
            })
            await controller.initialize()
            if (retainedPhase === 'sourceUnavailable') await controller.resume()
            if (retainedPhase === 'refreshPending') {
                await expect(controller.sync('pairing-refresh')).rejects.toThrow('refresh failed')
            }

            await expect(controller.prepare()).rejects.toThrow('retained peer sync operation')
            await expect(controller.start('session-blocked')).rejects.toThrow('retained peer sync operation')
            expect(prepare).not.toHaveBeenCalled()
            expect(start).not.toHaveBeenCalled()
        },
    )

    it.each(['completed', 'refreshPending'] as const)(
        'continues to reject fresh pairing sync while %s is retained',
        async (retainedPhase) => {
            const completed: PeerBidirectionalSyncResult = {
                kind: 'noChanges',
                operationId: 'operation-blocked',
                revision: 7,
                remoteRevision: 7,
                transferredObjects: 0,
                transferredBytes: 0,
                backups: [],
            }
            const sync = vi.fn(async () => {
                if (retainedPhase === 'refreshPending') {
                    throw new PeerBidirectionalRefreshError(completed, new Error('refresh failed'))
                }
                return completed
            })
            const controller = createPeerBidirectionalController({
                facade: facade({
                    sync,
                    status: async () => ({
                        source: { phase: 'idle', devices: [] },
                        operation: retainedPhase === 'completed'
                            ? { phase: 'completed', result: completed }
                            : undefined,
                    }),
                }),
            })
            await controller.initialize()
            if (retainedPhase === 'refreshPending') {
                await expect(controller.sync('pairing-original')).rejects.toThrow('refresh failed')
            }

            await expect(controller.sync('pairing-fresh')).rejects.toThrow('retained peer sync operation')
        },
    )
})
