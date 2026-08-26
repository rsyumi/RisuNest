import { describe, expect, it } from 'vitest'

import {
    authorizeConflictReplacement,
    type ConflictReplacementRequest,
} from './conflictBackupGate'
import type { BidirectionalSyncConflictPlan } from './bidirectionalSyncPlan'

const plan: BidirectionalSyncConflictPlan = {
    kind: 'conflict',
    libraryId: 'library-a',
    expectedLocalRevision: 4,
    expectedRemoteGeneration: 'remote-generation',
    baseManifestHash: '11'.repeat(32),
    localManifestHash: '22'.repeat(32),
    remoteManifestHash: '33'.repeat(32),
    conflicts: [{ key: 'r1:root', type: 'same-record' }],
    replacementAllowed: false,
    requiredBackup: 'complete-lossless-package',
    contentBytes: 0,
}

function request(
    winner: ConflictReplacementRequest['winner'],
    backup: ConflictReplacementRequest['backup'],
): ConflictReplacementRequest {
    return { plan, winner, backup }
}

describe('authorizeConflictReplacement', () => {
    it('blocks replacement until a verified losing-side lossless package exists', () => {
        expect(authorizeConflictReplacement(request('local', undefined))).toEqual({
            kind: 'blocked',
            reason: 'lossless-backup-required',
            losingSide: 'remote',
            requiredManifestHash: plan.remoteManifestHash,
        })
    })

    it('rejects a structurally forged proof until the J2 verifier issues an opaque proof', () => {
        const forged = {
            packageId: 'lossless-package-1',
            libraryId: 'library-a',
            sourceManifestHash: plan.remoteManifestHash,
            verifiedComponents: ['database', 'assets', 'inlays', 'cold'],
        } as unknown as ConflictReplacementRequest['backup']

        expect(() => authorizeConflictReplacement(request('local', forged)))
            .toThrow('J2 lossless verifier')
    })
})
