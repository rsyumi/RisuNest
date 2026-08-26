import { describe, expect, it, vi } from 'vitest'

import type {
    AccountNativeOfficialWriteAttempt,
    AccountNativeOfficialWriteResult,
    AccountStorage,
} from '../accountStorage'
import type {
    NativeOfficialPublicationAttemptResult,
    NativeOfficialPublicationReceipt,
} from '../nativeFileJobs'
import {
    nativePersistentRevisionLease,
    type NativePersistentRevisionLease,
} from '../nativePersistentExport'
import { createNativeOfficialPublicationJobPublisher } from './nativeOfficialPublicationJob'

function pinnedLease(): NativePersistentRevisionLease {
    return {
        revision: 7,
        [nativePersistentRevisionLease]: 'snapshot-publication-1',
        readRoot: vi.fn(),
        queryPresets: vi.fn(),
        readPreset: vi.fn(),
        queryCharacters: vi.fn(),
        readCharacter: vi.fn(),
        queryConversations: vi.fn(),
        readConversation: vi.fn(),
        readConversationWindow: vi.fn(),
        queryPluginStorage: vi.fn(),
        readPluginStorage: vi.fn(),
        readAssetAlias: vi.fn(),
        readAssetOwnerHead: vi.fn(),
        release: vi.fn(),
    }
}

function receipt(
    publication: NativeOfficialPublicationAttemptResult,
    acknowledge = vi.fn(async () => undefined),
): NativeOfficialPublicationReceipt {
    return {
        jobId: 'publication-1',
        result: {
            revision: 7,
            sourceBytes: 128,
            sourceSha256: 'a'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
            publication,
        },
        acknowledge,
    }
}

function accountHarness() {
    const completeReload = vi.fn(async () => undefined)
    const account: Pick<AccountStorage, 'writeOfficialDatabaseFromNative'> = {
        async writeOfficialDatabaseFromNative<T>(
            attempt: AccountNativeOfficialWriteAttempt<T>,
            options?: { signal?: AbortSignal },
        ): Promise<AccountNativeOfficialWriteResult<T> | null> {
            const attempted = await attempt({
                credential: { kind: 'risu-auth', token: 'legacy-token' },
                session: 'session-1',
                saveDate: '1700000000000',
                signal: options?.signal,
            })
            if (attempted === null) return null
            if (attempted.kind === 'auth-warning') return { kind: 'auth-warning' }
            if (attempted.kind === 'reauthentication-needed') {
                throw new Error('Harness does not retry reauthentication')
            }
            return {
                kind: attempted.kind,
                replacementKey: attempted.replacementKey,
                receipt: attempted.receipt,
                completeReload,
            }
        },
    }
    return { account, completeReload }
}

describe('native official publication job publisher', () => {
    it('passes only the pinned revision and exact projection, retaining success until finalization', async () => {
        const events: string[] = []
        const acknowledge = vi.fn(async () => { events.push('acknowledge') })
        const terminal = receipt({
            kind: 'written',
            accountId: 'account-1',
            session: 'session-2',
            saveDate: '1700000000000',
            status: 200,
            replacementKey: 'database/database.bin',
            warning: null,
            reloadSession: true,
        }, acknowledge)
        const runAttempt = vi.fn(async () => {
            events.push('attempt')
            return terminal
        })
        const reconcilePendingPublications = vi.fn(async () => {
            events.push('reconcile')
            return null
        })
        const harness = accountHarness()
        const signal = new AbortController().signal
        const publish = createNativeOfficialPublicationJobPublisher({
            ...harness,
            baseUrl: 'https://hub.invalid',
            runAttempt,
            reconcilePendingPublications,
        })

        const result = await publish({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {
                'assets/local.png': 'assets/remote.png',
            },
            signal,
        })

        expect(runAttempt).toHaveBeenCalledWith({
            expectedRevision: 7,
            lease: 'snapshot-publication-1',
            accountId: 'account-1',
            baseUrl: 'https://hub.invalid',
            replacements: { 'assets/local.png': 'assets/remote.png' },
            session: 'session-1',
            saveDate: '1700000000000',
            credential: { kind: 'risu-auth', token: 'legacy-token' },
        }, { signal })
        expect(events).toEqual(['reconcile', 'attempt'])
        expect(result?.databaseFingerprint).toBe('a'.repeat(64))
        expect(acknowledge).not.toHaveBeenCalled()

        await result?.acknowledge()
        await result?.completeReload()

        expect(acknowledge).toHaveBeenCalledOnce()
        expect(harness.completeReload).toHaveBeenCalledOnce()
    })

    it('acknowledges consumed auth outcomes and returns capability fallback before a job exists', async () => {
        const authAcknowledge = vi.fn(async () => undefined)
        const authReceipt = receipt({
            kind: 'auth-warning',
            accountId: 'account-1',
            session: null,
            saveDate: '1700000000000',
            status: 403,
        }, authAcknowledge)
        const authHarness = accountHarness()
        const authPublisher = createNativeOfficialPublicationJobPublisher({
            ...authHarness,
            baseUrl: 'https://hub.invalid',
            runAttempt: vi.fn(async () => authReceipt),
        })

        await expect(authPublisher({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
        })).rejects.toThrow('authorization warning')
        expect(authAcknowledge).toHaveBeenCalledOnce()

        const unavailableHarness = accountHarness()
        const unavailable = createNativeOfficialPublicationJobPublisher({
            ...unavailableHarness,
            baseUrl: 'https://hub.invalid',
            runAttempt: vi.fn(async () => null),
        })

        await expect(unavailable({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
        })).resolves.toBeNull()
    })

    it('falls back before account session work when the pinned lease is not native', async () => {
        const lease = pinnedLease()
        Reflect.deleteProperty(lease, nativePersistentRevisionLease)
        const harness = accountHarness()
        const writeOfficialDatabaseFromNative = vi.spyOn(
            harness.account,
            'writeOfficialDatabaseFromNative',
        )
        const runAttempt = vi.fn()
        const publish = createNativeOfficialPublicationJobPublisher({
            ...harness,
            baseUrl: 'https://hub.invalid',
            runAttempt,
        })

        await expect(publish({
            revision: 7,
            accountId: 'account-1',
            lease,
            resourceReplacements: {},
        })).resolves.toBeNull()

        expect(writeOfficialDatabaseFromNative).not.toHaveBeenCalled()
        expect(runAttempt).not.toHaveBeenCalled()
    })

    it('reuses a durably recovered matching publication instead of uploading it again', async () => {
        const recovered = {
            accountId: 'account-1',
            revision: 7,
            databaseFingerprint: 'b'.repeat(64),
        }
        const reconcilePendingPublications = vi.fn(async () => recovered)
        const runAttempt = vi.fn()
        const harness = accountHarness()
        const publish = createNativeOfficialPublicationJobPublisher({
            ...harness,
            baseUrl: 'https://hub.invalid',
            reconcilePendingPublications,
            runAttempt,
        })

        const result = await publish({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
        })

        expect(reconcilePendingPublications).toHaveBeenCalledWith({
            accountId: 'account-1',
            revision: 7,
        })
        expect(runAttempt).not.toHaveBeenCalled()
        expect(result?.databaseFingerprint).toBe(recovered.databaseFingerprint)
        await expect(result?.acknowledge()).resolves.toBeUndefined()
        await expect(result?.completeReload()).resolves.toBeUndefined()
    })
})
