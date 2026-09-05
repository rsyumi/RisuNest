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
    return { operation: undefined }
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
        status: async () => idleStatus(),
        syncRegistered: async () => ({
            kind: 'noChanges', operationId: 'operation-1', revision: 1, remoteRevision: 1,
            transferredObjects: 0, transferredBytes: 0, backups: [],
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
    it('reads durable target state once during initialization', async () => {
        const status = vi.fn(async () => ({
            operation: {
                phase: 'targetPrepared' as const,
                operationId: 'operation-target',
            },
        }))
        const controller = createPeerBidirectionalController({ facade: facade({ status }) })

        await controller.initialize()
        await controller.initialize()

        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'targetPrepared',
            operationId: 'operation-target',
        })
        expect(status).toHaveBeenCalledTimes(1)
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
        'keeps retained %s state when registered recovery fails',
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
            const syncRegistered = vi.fn(async () => { throw new Error('fresh peer unavailable') })
            const controller = createPeerBidirectionalController({
                facade: facade({
                    syncRegistered,
                    status: async () => ({
                        operation,
                    } as unknown as PeerBidirectionalStatus),
                }),
            })
            await controller.initialize()

            await expect(controller.syncRegistered('device-fresh')).rejects.toThrow('fresh peer unavailable')

            expect(syncRegistered).toHaveBeenCalledWith('device-fresh')
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
                          operation: { phase: 'completed', result: completed },
                      }
                    : ({
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

    it('shares one promise for the same device and rejects a different concurrent device', async () => {
        let finish!: (value: PeerBidirectionalSyncResult) => void
        const syncRegistered = vi.fn(() => new Promise<PeerBidirectionalSyncResult>((resolve) => {
            finish = resolve
        }))
        const controller = createPeerBidirectionalController({ facade: facade({ syncRegistered }) })
        const first = controller.syncRegistered('device-a')
        const same = controller.syncRegistered('device-a')
        await expect(controller.syncRegistered('device-b')).rejects.toThrow('different peer sync operation')
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
        expect(syncRegistered).toHaveBeenCalledTimes(1)
    })

    it('retains explicit conflicts until the user chooses one side', async () => {
        const resolveRegistered = vi.fn(async () => ({
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
                syncRegistered: async () => ({
                    kind: 'conflict',
                    operationId: 'operation-conflict',
                    conflicts: [{ key: 'r1:root', type: 'sameRecord' }],
                    localManifestHash: 'a'.repeat(64),
                    remoteManifestHash: 'b'.repeat(64),
                }),
                resolveRegistered,
            }),
        })

        await controller.syncRegistered('device-conflict')
        expect(controller.snapshot().operationPhase).toBe('awaitingConflict')
        await controller.resolveRegistered('device-conflict', 'local')
        expect(resolveRegistered).toHaveBeenCalledWith('device-conflict', 'operation-conflict', 'local')
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

    it('refuses a second registered choice while one conflict resolution runs', async () => {
        let finishResolve!: (value: PeerBidirectionalSyncResult) => void
        const resolveRegistered = vi.fn(() => new Promise<PeerBidirectionalSyncResult>((done) => {
            finishResolve = done
        }))
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
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
                resolveRegistered,
            }),
        })
        await controller.initialize()

        const first = controller.resolveRegistered('device-choice', 'local')
        // The choice leaves 'awaitingConflict' the moment it starts, so a second
        // call meets the conflict guard rather than the operation coalescer.
        await expect(controller.resolveRegistered('device-choice', 'remote'))
            .rejects.toThrow('No peer sync conflict is awaiting a choice')
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
        expect(resolveRegistered).toHaveBeenCalledTimes(1)
    })

    it('coalesces identical retained resume operations', async () => {
        let finishResume!: (value: PeerBidirectionalSyncResult) => void
        const resume = vi.fn(() => new Promise<PeerBidirectionalSyncResult>((done) => {
            finishResume = done
        }))
        const resumed = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
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
                    operation: { phase: 'awaitingConflict', result: conflict },
                }),
                resolveRegistered: async () => { throw new Error('resolve transport failed') },
            }),
        })
        await conflicted.initialize()
        await expect(conflicted.resolveRegistered('device-conflict-error', 'local'))
            .rejects.toThrow('resolve transport failed')
        expect(conflicted.snapshot()).toMatchObject({
            operationPhase: 'awaitingConflict',
            operationResult: conflict,
            operationError: 'resolve transport failed',
        })

        const committed = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
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
                    operation: { phase: 'awaitingConflict', result: conflict },
                }),
                syncRegistered: async () => { throw new Error('sync response lost') },
            }),
        })
        await synced.initialize()
        await expect(synced.syncRegistered('device-response-loss')).rejects.toThrow('sync response lost')
        expect(synced.snapshot()).toMatchObject({
            operationPhase: 'awaitingConflict',
            operationResult: conflict,
            operationError: 'sync response lost',
        })

        let resolveStatusCalls = 0
        const resolved = createPeerBidirectionalController({
            facade: facade({
                status: async () => resolveStatusCalls++ === 0 ? ({
                    operation: { phase: 'awaitingConflict', result: conflict },
                }) : ({
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-response-loss',
                        committedRevision: 6,
                    },
                }),
                resolveRegistered: async () => { throw new Error('resolve response lost') },
            }),
        })
        await resolved.initialize()
        await expect(resolved.resolveRegistered('device-response-loss', 'local'))
            .rejects.toThrow('resolve response lost')
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
                    operation: {
                        phase: 'localCommitted',
                        operationId: 'operation-response-loss',
                        committedRevision: 6,
                    },
                }) : ({
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
                              operation: { phase, operationId: `operation-${phase}` },
                          } as PeerBidirectionalStatus),
                    syncRegistered: async () => { throw new Error('sync failed before commit') },
                }),
            })
            await controller.initialize()

            await expect(controller.syncRegistered('device-precommit')).rejects.toThrow('sync failed before commit')

            expect(controller.snapshot()).toMatchObject({
                operationPhase: phase,
                operationId: `operation-${phase}`,
                operationResult: undefined,
                operationRetained: true,
                operationError: 'sync failed before commit',
            })
        },
    )

    it('projects source-unavailable as a recoverable retained operation', async () => {
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
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

    it('permits registered recovery for an awaiting conflict', async () => {
        const syncRegistered = vi.fn(async () => ({
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
                syncRegistered,
            }),
        })
        await controller.initialize()

        await expect(controller.syncRegistered('device-new')).resolves.toMatchObject({ kind: 'noChanges' })
        expect(syncRegistered).toHaveBeenCalledWith('device-new')
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
        const resolveRegistered = vi.fn(async () => {
            if (failRefresh) {
                failRefresh = false
                throw new PeerBidirectionalRefreshError(completed, new Error('renderer refresh failed'))
            }
            return completed
        })
        const controller = createPeerBidirectionalController({
            facade: facade({
                status: async () => ({
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
                resolveRegistered,
            }),
        })
        await controller.initialize()

        await expect(controller.resolveRegistered('device-refresh', 'local'))
            .rejects.toThrow('renderer refresh failed')
        expect(controller.snapshot()).toMatchObject({
            operationPhase: 'refreshPending',
            operationResult: completed,
            operationRetained: true,
        })
        await expect(controller.resume()).resolves.toBe(completed)
        expect(resolveRegistered).toHaveBeenCalledTimes(2)
        expect(resolveRegistered).toHaveBeenNthCalledWith(1, 'device-refresh', 'operation-refresh', 'local')
        expect(resolveRegistered).toHaveBeenNthCalledWith(2, 'device-refresh', 'operation-refresh', 'local')
        expect(controller.snapshot().operationPhase).toBe('completed')
    })

    it('acknowledges and clears a retained terminal result', async () => {
        const acknowledge = vi.fn(async () => undefined)
        const controller = createPeerBidirectionalController({ facade: facade({ acknowledge }) })
        await controller.syncRegistered('device-terminal')
        expect(controller.snapshot().operationPhase).toBe('completed')

        await controller.acknowledge()

        expect(acknowledge).toHaveBeenCalledWith('operation-1')
        expect(controller.snapshot()).toMatchObject({ operationPhase: 'idle', operationId: undefined })
    })

    it.each(['localCommitted', 'sourceUnavailable'] as const)(
        'abandons a retained %s operation and permits a new sync',
        async (retainedPhase) => {
            const acknowledge = vi.fn(async () => undefined)
            const syncRegistered = vi.fn(async () => ({
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
                    syncRegistered,
                    status: async () => ({
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
            await controller.syncRegistered('device-next')
            expect(syncRegistered).toHaveBeenCalledWith('device-next')
        },
    )

    it.each(['localCommitted', 'sourceUnavailable'] as const)(
        'uses a registered device to recover a retained %s operation',
        async (retainedPhase) => {
            const syncRegistered = vi.fn(facade().syncRegistered)
            const controller = createPeerBidirectionalController({
                facade: facade({
                    syncRegistered,
                    status: async () => ({
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

            await expect(controller.syncRegistered('device-fresh')).resolves.toMatchObject({ kind: 'noChanges' })

            expect(syncRegistered).toHaveBeenCalledWith('device-fresh')
            expect(controller.snapshot().operationPhase).toBe('completed')
        },
    )
})

describe('peer bidirectional controller surface', () => {
    it('exposes only the target side', () => {
        const controller = createPeerBidirectionalController({ facade: facade() })

        expect(Object.keys(controller).sort()).toEqual([
            'abandon', 'acknowledge', 'initialize', 'resolveRegistered', 'resume',
            'snapshot', 'subscribe', 'syncRegistered',
        ])
        expect(controller.snapshot()).toEqual({
            operationPhase: 'idle', operationRetained: false, operationError: '',
        })
    })
})
