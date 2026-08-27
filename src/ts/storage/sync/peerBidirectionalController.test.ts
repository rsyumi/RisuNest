import { describe, expect, it, vi } from 'vitest'

import { createPeerBidirectionalController } from './peerBidirectionalController'
import type {
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
        resolve: async () => ({
            kind: 'noChanges',
            operationId: 'operation-1',
            revision: 1,
            remoteRevision: 1,
            transferredObjects: 0,
            transferredBytes: 0,
            backups: [],
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

    it('rejects source and new sync commands while an operation is retained', async () => {
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
        await expect(controller.sync('pairing-new')).rejects.toThrow('retained peer sync operation')
        expect(prepare).not.toHaveBeenCalled()
        expect(sync).not.toHaveBeenCalled()
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
})
