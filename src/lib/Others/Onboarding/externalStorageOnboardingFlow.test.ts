import { describe, expect, it } from 'vitest'
import { mergeExternalHistoryItems } from 'src/ts/storage/sync/external/connection'
import type {
    ExternalConflictSummary,
    ExternalConnectionSummary,
    ExternalHistoryItem,
} from 'src/ts/storage/sync/external/types'
import {
    externalOnboardingAction,
    externalOnboardingConflictStep,
    externalOnboardingRestorable,
    externalOnboardingRestoreAreas,
    externalOnboardingRestoreRestarts,
    externalOnboardingSyncOutcome,
} from './externalStorageOnboardingFlow'

function connection(
    purpose: 'backup' | 'sync',
): Pick<ExternalConnectionSummary, 'purpose'> {
    return { purpose }
}

function item(
    id: string,
    createdAtMs: string,
    overrides: Partial<ExternalHistoryItem> = {},
): ExternalHistoryItem {
    return {
        id,
        kind: 'recovery-candidate',
        createdAtMs: createdAtMs as ExternalHistoryItem['createdAtMs'],
        logicalRevision: '1' as ExternalHistoryItem['logicalRevision'],
        pinned: false,
        complete: true,
        verified: true,
        includedSections: [],
        sameDevice: false,
        ...overrides,
    }
}

function conflict(
    overrides: Partial<ExternalConflictSummary> = {},
): ExternalConflictSummary {
    return {
        id: 'conflict-1',
        connectionId: 'connection-1',
        detectedAtMs: '1' as ExternalConflictSummary['detectedAtMs'],
        localRevision: '2' as ExternalConflictSummary['localRevision'],
        remoteRevision: '9' as ExternalConflictSummary['remoteRevision'],
        localAvailable: true,
        remoteAvailable: true,
        remotePointConfirmed: true,
        resolved: false,
        ...overrides,
    }
}

describe('external storage onboarding action', () => {
    it('joins a synchronization repository and reads a backup repository back', () => {
        expect(externalOnboardingAction(connection('sync'))).toBe('sync')
        expect(externalOnboardingAction(connection('backup'))).toBe('restore')
    })

    it('replaces only what a backup covers, so no restore restarts the app', () => {
        expect(externalOnboardingRestoreAreas(item('one', '1'))).toEqual([
            'library',
            'referencedAssets',
        ])
        expect(
            externalOnboardingRestoreAreas(
                item('two', '1', {
                    includedSections: ['hypa', 'local-plugins', 'local-settings'],
                }),
            ),
        ).toEqual(['library', 'referencedAssets', 'hypa', 'local-plugins'])
        expect(
            externalOnboardingRestoreAreas(
                item('three', '1', {
                    includedSections: ['hypa', 'local-settings'],
                    sameDevice: true,
                }),
            ),
        ).toEqual(['library', 'referencedAssets', 'hypa', 'local-settings'])
        expect(externalOnboardingRestoreRestarts()).toBe(false)
    })
})

describe('external storage onboarding backups', () => {
    it('offers the newest readable backup first', () => {
        const merged = mergeExternalHistoryItems([], [
            item('older', '1000'),
            item('newest', '3000'),
            item('middle', '2000'),
        ])

        expect(externalOnboardingRestorable(merged).map(entry => entry.id))
            .toEqual(['newest', 'middle', 'older'])
    })

    it('leaves out entries a restore cannot read back', () => {
        const merged = mergeExternalHistoryItems([], [
            item('partial', '3000', { complete: false }),
            item('unverified', '2000', { verified: false }),
            item('usable', '1000'),
        ])

        expect(externalOnboardingRestorable(merged).map(entry => entry.id)).toEqual(['usable'])
    })
})

describe('external storage onboarding first synchronization', () => {
    it('reads the repository side before it can be chosen', () => {
        expect(externalOnboardingConflictStep(conflict({
            remotePointConfirmed: false,
        }))).toBe('receive-repository')
        expect(externalOnboardingConflictStep(conflict())).toBe('take-repository')
        expect(externalOnboardingConflictStep(conflict({ remoteAvailable: false }))).toBe('wait')
    })

    it('separates a finished attempt, a first-attach decision and a failure', () => {
        expect(externalOnboardingSyncOutcome({ kind: 'complete' })).toBe('complete')
        expect(externalOnboardingSyncOutcome({
            kind: 'blocked',
            reason: 'external-storage-conflict',
        })).toBe('conflict')
        expect(externalOnboardingSyncOutcome({ kind: 'blocked', reason: 'transient' })).toBe('error')
        expect(externalOnboardingSyncOutcome({ kind: 'cancelled' })).toBe('error')
    })
})
