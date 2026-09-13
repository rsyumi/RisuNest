import type { AccountStorage } from '../accountStorage'
import {
    cancelNativeOfficialPublication,
    continueNativeOfficialPublication,
    runNativeOfficialPublicationAttempt,
    type NativeFileJobOptions,
    type NativeOfficialPublicationReceipt,
    type NativeOfficialPublicationRequest,
    type NativeOfficialPublicationRetryRequest,
    type NativeOfficialPublicationRunResult,
} from '../nativeFileJobs'
import {
    hasNativePersistentRevisionLease,
    nativePersistentRevisionLease,
} from '../nativePersistentExport'
import type {
    OfficialNativeDatabasePublisher,
    OfficialRecoveredPublication,
} from './officialAccountSnapshot'

export interface NativeOfficialPublicationJobDependencies {
    account: Pick<AccountStorage, 'writeOfficialDatabaseFromNative'>
    baseUrl: string
    runAttempt?(
        request: NativeOfficialPublicationRequest,
        options?: NativeFileJobOptions,
    ): Promise<NativeOfficialPublicationRunResult | null>
    continueAttempt?(
        jobId: string,
        request: NativeOfficialPublicationRetryRequest,
        expected: {
            revision: number
            accountId: string
        },
        options?: NativeFileJobOptions,
    ): Promise<NativeOfficialPublicationRunResult>
    cancelAttempt?(jobId: string): Promise<NativeOfficialPublicationReceipt | null>
    reconcilePendingPublications?(input: {
        accountId: string
        revision: number
    }): Promise<OfficialRecoveredPublication | null>
}

function hasDrainedNativeOfficialPublicationCancellation(error: unknown): boolean {
    return typeof error === 'object'
        && error !== null
        && 'nativeOfficialPublicationCancellationDrained' in error
        && error.nativeOfficialPublicationCancellationDrained === true
}

export function createNativeOfficialPublicationJobPublisher(
    dependencies: NativeOfficialPublicationJobDependencies,
): OfficialNativeDatabasePublisher {
    const runAttempt = dependencies.runAttempt ?? runNativeOfficialPublicationAttempt
    const continueAttempt = dependencies.continueAttempt ?? continueNativeOfficialPublication
    const cancelAttempt = dependencies.cancelAttempt ?? cancelNativeOfficialPublication
    return async (input) => {
        const recovered = await dependencies.reconcilePendingPublications?.({
            accountId: input.accountId,
            revision: input.revision,
        })
        if (recovered) {
            return {
                databaseFingerprint: recovered.databaseFingerprint,
                acknowledge: async () => undefined,
                completeReload: async () => undefined,
            }
        }
        if (!hasNativePersistentRevisionLease(input.lease)) return null
        let pendingJobId: string | null = null
        let result
        try {
            result = await dependencies.account.writeOfficialDatabaseFromNative(
                async (context) => {
                    const outcome = pendingJobId
                        ? await continueAttempt(
                            pendingJobId,
                            {
                                accountId: input.accountId,
                                session: context.session,
                                saveDate: context.saveDate,
                                credential: context.credential,
                            },
                            {
                                revision: input.revision,
                                accountId: input.accountId,
                            },
                            { signal: context.signal },
                        )
                        : await runAttempt({
                            expectedRevision: input.revision,
                            lease: input.lease[nativePersistentRevisionLease],
                            accountId: input.accountId,
                            baseUrl: dependencies.baseUrl,
                            replacements: input.resourceReplacements,
                            session: context.session,
                            saveDate: context.saveDate,
                            credential: context.credential,
                        }, { signal: context.signal })
                    if (outcome === null) return null
                    if (outcome.kind === 'waiting-for-reauthentication') {
                        pendingJobId = outcome.jobId
                        return {
                            kind: 'reauthentication-needed',
                            session: outcome.session,
                            warning: outcome.warning,
                        }
                    }
                    pendingJobId = null
                    const receipt = outcome.receipt
                    const publication = receipt.result.publication
                    if (publication.kind === 'auth-warning') {
                        await receipt.acknowledge()
                        return {
                            kind: 'auth-warning',
                            session: publication.session,
                            warning: publication.warning,
                        }
                    }
                    if (publication.kind === 'reauthentication-needed') {
                        await receipt.acknowledge()
                        throw new Error(
                            'Native official publication ended before reauthentication retry',
                        )
                    }
                    return {
                        kind: publication.kind,
                        session: publication.session,
                        replacementKey: publication.replacementKey,
                        warning: publication.kind === 'written' ? publication.warning : null,
                        reloadSession: publication.kind === 'written'
                            ? publication.reloadSession
                            : false,
                        receipt,
                    }
                },
                { signal: input.signal },
            )
        }
        catch (error) {
            if (
                pendingJobId !== null
                && !hasDrainedNativeOfficialPublicationCancellation(error)
            ) {
                try {
                    await cancelAttempt(pendingJobId)
                }
                catch {}
            }
            throw error
        }
        if (result === null) {
            if (pendingJobId === null) return null
            try {
                await cancelAttempt(pendingJobId)
            }
            catch {}
            throw new Error('Native official publication ended before reauthentication retry')
        }
        if (result.kind === 'auth-warning') {
            throw new Error('Official account authorization warning while writing database/database.bin')
        }
        return {
            databaseFingerprint: result.receipt.result.sourceSha256,
            acknowledge: result.receipt.acknowledge,
            completeReload: result.completeReload,
        }
    }
}
