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
