import { describe, expect, it } from 'vitest'

import type { ServerSyncSnapshot } from 'src/ts/storage/sync/serverSyncController'
import { serverSyncOnboardingOutcome } from './serverSyncOnboardingFlow'

const head = {
    libraryId: 'library',
    epoch: 'epoch',
    seq: '1',
    headId: 'head',
    minRetainedSeq: '0',
}
const identity = {
    endpoint: 'https://sync.example/',
    libraryId: 'library',
    deviceId: 'device',
}
function finished(phase: 'idle' | 'pending' | 'conflict'): ServerSyncSnapshot {
    return {
        running: false,
        paused: false,
        error: '',
        attemptId: 1,
        attemptIdentity: identity,
        initialSyncComplete: phase === 'idle',
        status: {
            localRevision: 3,
            reconciling: false,
            configured: true,
            ...identity,
            head,
            dirtyRecords: 0,
            fullScan: false,
            registrationRequired: false,
            operationPending: phase === 'pending',
        },
        result: {
            endpoint: identity.endpoint,
            phase,
            localRevision: 3,
            head,
            conflictCount: phase === 'conflict' ? 1 : 0,
            conflicts: [],
            appliedRecords: 0,
            proposedRecords: 0,
        },
    }
}

describe('sync server onboarding outcome', () => {
    it('keeps the progress screen while the attempt runs', () => {
        expect(serverSyncOnboardingOutcome({ ...finished('idle'), running: true })).toBe(
            'syncing',
        )
    })

    it('finishes only on the verified completion of the attempt', () => {
        expect(serverSyncOnboardingOutcome(finished('idle'))).toBe('complete')
        const stale = finished('idle')
        stale.status!.dirtyRecords = 1
        expect(serverSyncOnboardingOutcome(stale)).toBe('error')
    })

    it('offers the conflict choice when the first comparison found one', () => {
        expect(serverSyncOnboardingOutcome(finished('conflict'))).toBe('conflict')
    })

    it('treats a stop the reader asked for as done, not as a failure', () => {
        expect(
            serverSyncOnboardingOutcome({
                ...finished('idle'),
                paused: true,
                error: 'cancelled',
                initialSyncComplete: false,
            }),
        ).toBe('paused')
    })

    it('separates an unfinished server job from a failure', () => {
        expect(serverSyncOnboardingOutcome(finished('pending'))).toBe('pending')
        expect(
            serverSyncOnboardingOutcome({ ...finished('pending'), error: 'server-unreachable' }),
        ).toBe('error')
    })
})
