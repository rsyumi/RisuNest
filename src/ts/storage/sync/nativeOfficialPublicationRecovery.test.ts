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

    it('never adopts a different-account or authentication outcome', async () => {
        const mismatchAcknowledge = vi.fn(async () => undefined)
        const mismatch = writtenReceipt(mismatchAcknowledge)
        mismatch.result.publication.accountId = 'account-2'
        const authAcknowledge = vi.fn(async () => undefined)
        const auth: NativeOfficialPublicationReceipt = {
            ...writtenReceipt(authAcknowledge),
            jobId: 'publication-auth',
            result: {
                ...writtenReceipt(authAcknowledge).result,
                publication: {
                    kind: 'reauthentication-needed',
                    accountId: 'account-1',
                    session: null,
                    saveDate: '1700000000001',
                    status: 403,
                },
            },
        }
        const adoptPublishedRevision = vi.fn(async () => undefined)
        const adoptRecoveredOfficialWrite = vi.fn(() => ({
            completeReload: vi.fn(async () => undefined),
        }))
        const receipts = new Map([
            ['publication-mismatch', mismatch],
            ['publication-auth', auth],
        ])
        const recovery = createNativeOfficialPublicationRecovery(
            ['publication-mismatch', 'publication-auth'],
            {
                activeAccountId: () => 'account-1',
                account: { adoptRecoveredOfficialWrite },
                adapter: { adoptPublishedRevision },
                flushMetadata: vi.fn(async () => undefined),
                listJobIds: vi.fn(async () => []),
                resumeJob: vi.fn(async (jobId) => receipts.get(jobId) ?? null),
            },
        )

        await recovery.reconcile()

        expect(mismatchAcknowledge).toHaveBeenCalledOnce()
        expect(authAcknowledge).toHaveBeenCalledOnce()
        expect(adoptPublishedRevision).not.toHaveBeenCalled()
        expect(adoptRecoveredOfficialWrite).toHaveBeenCalledOnce()
        expect(adoptRecoveredOfficialWrite).toHaveBeenCalledWith({ session: null })
    })
})
