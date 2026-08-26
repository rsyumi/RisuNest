import type { AccountStorage } from '../accountStorage'
import {
    runNativeOfficialPublicationAttempt,
    type NativeFileJobOptions,
    type NativeOfficialPublicationReceipt,
    type NativeOfficialPublicationRequest,
} from '../nativeFileJobs'
import type { OfficialNativeDatabasePublisher } from './officialAccountSnapshot'

export interface NativeOfficialPublicationJobDependencies {
    account: Pick<AccountStorage, 'writeOfficialDatabaseFromNative'>
    baseUrl: string
    runAttempt?(
        request: NativeOfficialPublicationRequest,
        options?: NativeFileJobOptions,
    ): Promise<NativeOfficialPublicationReceipt | null>
    reconcilePendingPublications?(): Promise<void>
}

export function createNativeOfficialPublicationJobPublisher(
    dependencies: NativeOfficialPublicationJobDependencies,
): OfficialNativeDatabasePublisher {
    const runAttempt = dependencies.runAttempt ?? runNativeOfficialPublicationAttempt
    return async (input) => {
        await dependencies.reconcilePendingPublications?.()
        const result = await dependencies.account.writeOfficialDatabaseFromNative(
            async (context) => {
                const receipt = await runAttempt({
                    expectedRevision: input.revision,
                    accountId: input.accountId,
                    baseUrl: dependencies.baseUrl,
                    replacements: input.resourceReplacements,
                    session: context.session,
                    saveDate: context.saveDate,
                    credential: context.credential,
                }, { signal: context.signal })
                if (!receipt) return null
                const publication = receipt.result.publication
                if (
                    publication.kind === 'auth-warning'
                    || publication.kind === 'reauthentication-needed'
                ) {
                    await receipt.acknowledge()
                    return {
                        kind: publication.kind,
                        session: publication.session,
                    }
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
        if (result === null) return null
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
