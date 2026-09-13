import { describe, expect, it, vi } from 'vitest'

import type { NativeOfficialPublicationReceipt } from '../nativeFileJobs'
import { createNativeOfficialPublicationRecovery } from './nativeOfficialPublicationRecovery'

function writtenReceipt(
    acknowledge: () => Promise<void>,
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
            publication: {
                kind: 'written',
                accountId: 'account-1',
                session: 'session-2',
                saveDate: '1700000000000',
                status: 200,
                replacementKey: 'database/database.bin',
                warning: 'server warning',
                reloadSession: true,
            },
        },
        acknowledge,
    }
}

describe('native official publication recovery', () => {
    it('finalizes a recovered remote commit before acknowledging and reloading', async () => {
        const events: string[] = []
        let acknowledged = false
        const receipt = writtenReceipt(vi.fn(async () => {
            events.push('acknowledge')
            acknowledged = true
        }))
        const resumeJob = vi.fn(async () => receipt)
        const recovery = createNativeOfficialPublicationRecovery([], {
            activeAccountId: () => 'account-1',
            account: {
                adoptRecoveredOfficialWrite: vi.fn(() => {
                    events.push('session')
                    return { completeReload: async () => { events.push('reload') } }
                }),
            },
            adapter: {
                adoptPublishedRevision: vi.fn(async () => { events.push('association') }),
            },
            flushMetadata: vi.fn(async () => { events.push('metadata-flush') }),
            listJobIds: vi.fn(async () => acknowledged ? [] : ['publication-1']),
            resumeJob,
        })

        await recovery.reconcile()

        expect(events).toEqual([
            'session',
            'association',
            'metadata-flush',
            'acknowledge',
            'reload',
        ])
        expect(recovery.hasPending()).toBe(false)
        expect(recovery.takeRecoveredPublication('account-1', 7)).toEqual({
            accountId: 'account-1',
            revision: 7,
            databaseFingerprint: 'a'.repeat(64),
        })
        expect(recovery.takeRecoveredPublication('account-1', 7)).toBeNull()

        await recovery.reconcile()
        expect(resumeJob).toHaveBeenCalledOnce()
    })

    it('retains an unflushed receipt and retries only durable local finalization', async () => {
        const acknowledge = vi.fn(async () => undefined)
        const receipt = writtenReceipt(acknowledge)
        let flushAttempts = 0
        const resumeJob = vi.fn(async () => receipt)
        const recovery = createNativeOfficialPublicationRecovery(['publication-1'], {
            activeAccountId: () => 'account-1',
            account: {
                adoptRecoveredOfficialWrite: vi.fn(() => ({
                    completeReload: vi.fn(async () => undefined),
                })),
            },
            adapter: {
                adoptPublishedRevision: vi.fn(async () => undefined),
            },
            flushMetadata: vi.fn(async () => {
                if (flushAttempts++ === 0) throw new Error('metadata unavailable')
            }),
            listJobIds: vi.fn(async () => ['publication-1']),
            resumeJob,
        })

        await expect(recovery.reconcile()).rejects.toThrow('metadata unavailable')
        expect(acknowledge).not.toHaveBeenCalled()
        expect(recovery.hasPending()).toBe(true)

        await expect(recovery.reconcile()).resolves.toBeUndefined()
        expect(acknowledge).toHaveBeenCalledOnce()
        expect(resumeJob).toHaveBeenCalledTimes(2)
        expect(recovery.hasPending()).toBe(false)
    })

    it('retains successful receipts until their account can finalize them exactly once', async () => {
        let activeAccountId: string | null = null
        const writtenAcknowledge = vi.fn(async () => undefined)
        const written = writtenReceipt(writtenAcknowledge)
        const notModifiedAcknowledge = vi.fn(async () => undefined)
        const notModifiedBase = writtenReceipt(notModifiedAcknowledge)
        const notModified: NativeOfficialPublicationReceipt = {
            ...notModifiedBase,
            jobId: 'publication-not-modified',
            result: {
                ...notModifiedBase.result,
                revision: 8,
                sourceSha256: 'b'.repeat(64),
                publication: {
                    kind: 'not-modified',
                    accountId: 'account-2',
                    session: null,
                    saveDate: '1700000000001',
                    status: 304,
                    replacementKey: 'database/database.bin',
                },
            },
        }
        const adoptPublishedRevision = vi.fn(async () => undefined)
        const completeReload = vi.fn(async () => undefined)
        const adoptRecoveredOfficialWrite = vi.fn(() => ({
            completeReload,
        }))
        const receipts = new Map([
            ['publication-written', written],
            ['publication-not-modified', notModified],
        ])
        const recovery = createNativeOfficialPublicationRecovery(
            ['publication-written', 'publication-not-modified'],
            {
                activeAccountId: () => activeAccountId,
                account: { adoptRecoveredOfficialWrite },
                adapter: { adoptPublishedRevision },
                flushMetadata: vi.fn(async () => undefined),
                listJobIds: vi.fn(async () => []),
                resumeJob: vi.fn(async (jobId) => receipts.get(jobId) ?? null),
            },
        )

        await recovery.reconcile()

        expect(writtenAcknowledge).not.toHaveBeenCalled()
        expect(notModifiedAcknowledge).not.toHaveBeenCalled()
        expect(adoptPublishedRevision).not.toHaveBeenCalled()
        expect(adoptRecoveredOfficialWrite).not.toHaveBeenCalled()
        expect(recovery.hasPending()).toBe(true)

        activeAccountId = 'account-1'
        await recovery.reconcile()

        expect(writtenAcknowledge).toHaveBeenCalledOnce()
        expect(notModifiedAcknowledge).not.toHaveBeenCalled()
        expect(recovery.hasPending()).toBe(true)

        activeAccountId = 'account-2'
        await recovery.reconcile()
        await recovery.reconcile()

        expect(writtenAcknowledge).toHaveBeenCalledOnce()
        expect(notModifiedAcknowledge).toHaveBeenCalledOnce()
        expect(adoptPublishedRevision).toHaveBeenCalledTimes(2)
        expect(adoptRecoveredOfficialWrite).toHaveBeenCalledTimes(2)
        expect(completeReload).toHaveBeenCalledTimes(2)
        expect(recovery.hasPending()).toBe(false)
    })

    it('consumes authentication outcomes without adopting a publication', async () => {
        const acknowledge = vi.fn(async () => undefined)
        const base = writtenReceipt(acknowledge)
        const auth: NativeOfficialPublicationReceipt = {
            ...base,
            jobId: 'publication-auth',
            result: {
                ...base.result,
                publication: {
                    kind: 'reauthentication-needed',
                    warning: 'please sign in again',
                    accountId: 'account-1',
                    session: null,
                    saveDate: '1700000000001',
                    status: 403,
                },
            },
        }
        const mismatchAcknowledge = vi.fn(async () => undefined)
        const mismatchBase = writtenReceipt(mismatchAcknowledge)
        const mismatchAuth: NativeOfficialPublicationReceipt = {
            ...mismatchBase,
            jobId: 'publication-auth-mismatch',
            result: {
                ...mismatchBase.result,
                publication: {
                    kind: 'auth-warning',
                    warning: null,
                    accountId: 'account-2',
                    session: 'session-3',
                    saveDate: '1700000000002',
                    status: 403,
                },
            },
        }
        const adoptPublishedRevision = vi.fn(async () => undefined)
        const adoptRecoveredOfficialWrite = vi.fn(() => ({
            completeReload: vi.fn(async () => undefined),
        }))
        const receipts = new Map([
            ['publication-auth', auth],
            ['publication-auth-mismatch', mismatchAuth],
        ])
        const recovery = createNativeOfficialPublicationRecovery([...receipts.keys()], {
            activeAccountId: () => 'account-1',
            account: { adoptRecoveredOfficialWrite },
            adapter: { adoptPublishedRevision },
            flushMetadata: vi.fn(async () => undefined),
            listJobIds: vi.fn(async () => []),
            resumeJob: vi.fn(async (jobId) => receipts.get(jobId) ?? null),
        })

        await recovery.reconcile()

        expect(acknowledge).toHaveBeenCalledOnce()
        expect(mismatchAcknowledge).toHaveBeenCalledOnce()
        expect(adoptPublishedRevision).not.toHaveBeenCalled()
        expect(adoptRecoveredOfficialWrite).toHaveBeenCalledOnce()
        expect(adoptRecoveredOfficialWrite).toHaveBeenCalledWith({
            session: null,
            warning: 'please sign in again',
        })
        expect(recovery.hasPending()).toBe(false)
    })
})
