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

    it('authorizes only a complete package for the exact losing manifest', () => {
        expect(authorizeConflictReplacement(request('local', {
            packageId: 'lossless-package-1',
            libraryId: 'library-a',
            sourceManifestHash: plan.remoteManifestHash,
            verifiedComponents: ['database', 'assets', 'inlays', 'cold'],
        }))).toEqual({
            kind: 'authorized',
            winner: 'local',
            losingSide: 'remote',
            packageId: 'lossless-package-1',
            sourceManifestHash: plan.remoteManifestHash,
        })
    })

    it('rejects an incomplete or wrong-side backup proof', () => {
        expect(() => authorizeConflictReplacement(request('local', {
            packageId: 'lossless-package-2',
            libraryId: 'library-a',
            sourceManifestHash: plan.remoteManifestHash,
            verifiedComponents: ['database', 'assets', 'inlays'],
        }))).toThrow('components')
        expect(() => authorizeConflictReplacement(request('remote', {
            packageId: 'lossless-package-3',
            libraryId: 'library-a',
            sourceManifestHash: plan.remoteManifestHash,
            verifiedComponents: ['database', 'assets', 'inlays', 'cold'],
        }))).toThrow('losing manifest')
    })
})
