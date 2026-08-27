import { describe, expect, it, vi } from 'vitest'

import { createPeerBidirectionalController } from './peerBidirectionalController'
import type {
    PeerBidirectionalFacade,
    PeerBidirectionalStatus,
    PeerBidirectionalSyncResult,
} from './peerBidirectional'

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
})

