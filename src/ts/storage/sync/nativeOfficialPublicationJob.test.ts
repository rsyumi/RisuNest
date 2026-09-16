import { describe, expect, it, vi } from 'vitest'

import type {
    AccountNativeOfficialWriteAttempt,
    AccountNativeOfficialWriteResult,
    AccountStorage,
} from '../accountStorage'
import {
    continueNativeOfficialPublication,
    type NativeFileJobOptions,
    type NativeOfficialPublicationAttemptResult,
    type NativeOfficialPublicationReceipt,
    type NativeOfficialPublicationRequest,
    type NativeOfficialPublicationRetryRequest,
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
        readCharacterSummary: vi.fn(async () => null),
        readCharacter: vi.fn(),
        queryConversations: vi.fn(),
        readConversation: vi.fn(),
        readConversationMetadata: vi.fn(),
        readConversationWindow: vi.fn(),
        queryPluginStorage: vi.fn(),
        readPluginStorage: vi.fn(),
        readAssetAlias: vi.fn(),
        readAssetAliasesByKeys: vi.fn(),
        listAssetAliases: vi.fn(),
        readAssetRepositoryAuthority: vi.fn(),
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
    const warnings: Array<string | null | undefined> = []
    const account: Pick<AccountStorage, 'writeOfficialDatabaseFromNative'> = {
        async writeOfficialDatabaseFromNative<T>(
            attempt: AccountNativeOfficialWriteAttempt<T>,
            options?: { signal?: AbortSignal },
        ): Promise<AccountNativeOfficialWriteResult<T> | null> {
            let attempted = await attempt({
                credential: { kind: 'risu-auth', token: 'legacy-token' },
                session: 'session-1',
                saveDate: '1700000000000',
                signal: options?.signal,
            })
            if (attempted === null) return null
            warnings.push(attempted.warning)
            if (attempted.kind === 'auth-warning') return { kind: 'auth-warning' }
            if (attempted.kind === 'reauthentication-needed') {
                attempted = await attempt({
                    credential: { kind: 'risu-auth', token: 'fresh-token' },
                    session: attempted.session,
                    saveDate: '1700000000001',
                    signal: options?.signal,
                })
                if (attempted === null) return null
                if (attempted.kind === 'auth-warning') return { kind: 'auth-warning' }
                if (attempted.kind === 'reauthentication-needed') {
                    throw new Error('Harness received repeated reauthentication')
                }
            }
            return {
                kind: attempted.kind,
                replacementKey: attempted.replacementKey,
                receipt: attempted.receipt,
                completeReload,
            }
        },
    }
    return { account, completeReload, warnings }
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
            return { kind: 'completed' as const, receipt: terminal }
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
        const authReceipt = receipt(
            {
                kind: 'auth-warning',
                warning: 'quota exceeded',
                accountId: 'account-1',
                session: null,
                saveDate: '1700000000000',
                status: 403,
            },
            authAcknowledge,
        )
        const authHarness = accountHarness()
        const authPublisher = createNativeOfficialPublicationJobPublisher({
            ...authHarness,
            baseUrl: 'https://hub.invalid',
            runAttempt: vi.fn(async () => ({
                kind: 'completed' as const,
                receipt: authReceipt,
            })),
        })

        await expect(authPublisher({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
        })).rejects.toThrow('authorization warning')
        expect(authAcknowledge).toHaveBeenCalledOnce()
        expect(authHarness.warnings).toEqual(['quota exceeded'])

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

    it('reauthenticates and continues the same publication job without reacquiring its lease', async () => {
        const terminal = receipt({
            kind: 'written',
            accountId: 'account-1',
            session: 'session-42',
            saveDate: '1700000000001',
            status: 200,
            replacementKey: 'database/database.bin',
            warning: null,
            reloadSession: false,
        })
        const runAttempt = vi.fn(async (_request: NativeOfficialPublicationRequest) => ({
            kind: 'waiting-for-reauthentication' as const,
            warning: 'please sign in',
            jobId: 'publication-1',
            accountId: 'account-1',
            session: 'session-42',
        }))
        const continueAttempt = vi.fn(async () => ({
            kind: 'completed' as const,
            receipt: terminal,
        }))
        const cancelAttempt = vi.fn(async () => null)
        const harness = accountHarness()
        const publish = createNativeOfficialPublicationJobPublisher({
            ...harness,
            baseUrl: 'https://hub.invalid',
            runAttempt,
            continueAttempt,
            cancelAttempt,
        })

        const result = await publish({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: { 'asset://old': 'asset://new' },
        })

        expect(harness.warnings).toEqual(['please sign in'])
        expect(runAttempt).toHaveBeenCalledOnce()
        expect(runAttempt.mock.calls[0][0]).toMatchObject({
            lease: 'snapshot-publication-1',
            credential: { kind: 'risu-auth', token: 'legacy-token' },
            saveDate: '1700000000000',
        })
        expect(continueAttempt).toHaveBeenCalledWith(
            'publication-1',
            {
                accountId: 'account-1',
                session: 'session-42',
                saveDate: '1700000000001',
                credential: { kind: 'risu-auth', token: 'fresh-token' },
            },
            { revision: 7, accountId: 'account-1' },
            { signal: undefined },
        )
        expect(cancelAttempt).not.toHaveBeenCalled()
        expect(result?.databaseFingerprint).toBe('a'.repeat(64))
    })

    it('cancels a pending native publication when reauthentication stops without retrying', async () => {
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
                expect(attempted).toMatchObject({ kind: 'reauthentication-needed' })
                return null
            },
        }
        const cancelAttempt = vi.fn(async () => null)
        const publish = createNativeOfficialPublicationJobPublisher({
            account,
            baseUrl: 'https://hub.invalid',
            runAttempt: vi.fn(async () => ({
                kind: 'waiting-for-reauthentication' as const,
                warning: null,
                jobId: 'publication-1',
                accountId: 'account-1',
                session: 'session-42',
            })),
            cancelAttempt,
        })

        await expect(publish({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
        })).rejects.toThrow('ended before reauthentication retry')

        expect(cancelAttempt).toHaveBeenCalledOnce()
        expect(cancelAttempt).toHaveBeenCalledWith('publication-1')
    })

    it('does not recancel a continuation that already drained an abort', async () => {
        const drainedAbort = Object.assign(
            new DOMException('Native file job was cancelled', 'AbortError'),
            { nativeOfficialPublicationCancellationDrained: true },
        )
        const continueAttempt = vi.fn(async () => { throw drainedAbort })
        const cancelAttempt = vi.fn(async () => null)
        const publish = createNativeOfficialPublicationJobPublisher({
            ...accountHarness(),
            baseUrl: 'https://hub.invalid',
            runAttempt: vi.fn(async () => ({
                kind: 'waiting-for-reauthentication' as const,
                warning: null,
                jobId: 'publication-1',
                accountId: 'account-1',
                session: 'session-42',
            })),
            continueAttempt,
            cancelAttempt,
        })

        await expect(publish({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
            signal: new AbortController().signal,
        })).rejects.toMatchObject({ name: 'AbortError' })

        expect(continueAttempt).toHaveBeenCalledOnce()
        expect(cancelAttempt).not.toHaveBeenCalled()
    })

    it('does not recancel an abort drained while polling an accepted continuation', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        const account: Pick<AccountStorage, 'writeOfficialDatabaseFromNative'> = {
            async writeOfficialDatabaseFromNative<T>(
                attempt: AccountNativeOfficialWriteAttempt<T>,
                options?: { signal?: AbortSignal },
            ): Promise<AccountNativeOfficialWriteResult<T> | null> {
                const first = await attempt({
                    credential: { kind: 'risu-auth', token: 'legacy-token' },
                    session: 'session-1',
                    saveDate: '1700000000000',
                    signal: options?.signal,
                })
                expect(first).toMatchObject({ kind: 'reauthentication-needed' })
                await attempt({
                    credential: { kind: 'risu-auth', token: 'fresh-token' },
                    session: first?.session ?? null,
                    saveDate: '1700000000001',
                    signal: options?.signal,
                })
                throw new Error('Continuation polling must abort')
            },
        }
        const continueAttempt = vi.fn(async (
            jobId: string,
            request: NativeOfficialPublicationRetryRequest,
            expected: { revision: number; accountId: string },
            options?: NativeFileJobOptions,
        ) => await continueNativeOfficialPublication(jobId, request, expected, options, {
            isTauri: () => true,
            invoke: async (command) => {
                commands.push(command)
                if (command === 'native_file_job_official_publication_retry') {
                    controller.abort(new DOMException('Publication cancelled', 'AbortError'))
                    return 'accepted'
                }
                if (command === 'native_file_job_cancel') return 'requested'
                if (command === 'native_file_job_status') return {
                    jobId: 'publication-1',
                    kind: 'official-publication-upload',
                    state: 'cancelled',
                    phase: 'complete',
                    progress: { completedBytes: 128, completedItems: 1 },
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            },
            wait: async () => undefined,
        }))
        const cancelAttempt = vi.fn(async () => null)
        const publish = createNativeOfficialPublicationJobPublisher({
            account,
            baseUrl: 'https://hub.invalid',
            runAttempt: vi.fn(async () => ({
                kind: 'waiting-for-reauthentication' as const,
                warning: null,
                jobId: 'publication-1',
                accountId: 'account-1',
                session: 'session-42',
            })),
            continueAttempt,
            cancelAttempt,
        })

        await expect(publish({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
            signal: controller.signal,
        })).rejects.toMatchObject({ name: 'AbortError' })

        expect(continueAttempt).toHaveBeenCalledOnce()
        expect(commands).toEqual([
            'native_file_job_official_publication_retry',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
        expect(cancelAttempt).not.toHaveBeenCalled()
    })

    it('preserves a too-late successful job when login fails after it is pending', async () => {
        const loginFailure = new DOMException('Login cancelled', 'AbortError')
        const account: Pick<AccountStorage, 'writeOfficialDatabaseFromNative'> = {
            async writeOfficialDatabaseFromNative<T>(
                attempt: AccountNativeOfficialWriteAttempt<T>,
                options?: { signal?: AbortSignal },
            ): Promise<AccountNativeOfficialWriteResult<T> | null> {
                await attempt({
                    credential: { kind: 'risu-auth', token: 'legacy-token' },
                    session: 'session-1',
                    saveDate: '1700000000000',
                    signal: options?.signal,
                })
                throw loginFailure
            },
        }
        const acknowledge = vi.fn(async () => undefined)
        const cancelAttempt = vi.fn(async () => receipt({
            kind: 'written',
            accountId: 'account-1',
            session: 'session-42',
            saveDate: '1700000000000',
            status: 200,
            replacementKey: 'database/database.bin',
            warning: null,
            reloadSession: false,
        }, acknowledge))
        const publish = createNativeOfficialPublicationJobPublisher({
            account,
            baseUrl: 'https://hub.invalid',
            runAttempt: vi.fn(async () => ({
                kind: 'waiting-for-reauthentication' as const,
                warning: null,
                jobId: 'publication-1',
                accountId: 'account-1',
                session: 'session-42',
            })),
            cancelAttempt,
        })

        await expect(publish({
            revision: 7,
            accountId: 'account-1',
            lease: pinnedLease(),
            resourceReplacements: {},
        })).rejects.toBe(loginFailure)

        expect(cancelAttempt).toHaveBeenCalledWith('publication-1')
        expect(acknowledge).not.toHaveBeenCalled()
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
