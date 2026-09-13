import type { AccountStorage } from '../accountStorage'
import { listNativeOfficialPublicationJobs } from '../nativeFileJobRecovery'
import {
    resumeNativeOfficialPublication,
    type NativeOfficialPublicationReceipt,
} from '../nativeFileJobs'
import type {
    OfficialAccountSnapshotAdapter,
    OfficialRecoveredPublication,
} from './officialAccountSnapshot'

export interface NativeOfficialPublicationRecoveryDependencies {
    activeAccountId(): string | null
    account: Pick<AccountStorage, 'adoptRecoveredOfficialWrite'>
    adapter: Pick<OfficialAccountSnapshotAdapter, 'adoptPublishedRevision'>
    flushMetadata(): Promise<void>
    listJobIds?(): Promise<string[]>
    resumeJob?(jobId: string): Promise<NativeOfficialPublicationReceipt | null>
}

export interface NativeOfficialPublicationRecovery {
    reconcile(): Promise<void>
    hasPending(): boolean
    takeRecoveredPublication(
        accountId: string,
        revision: number,
    ): OfficialRecoveredPublication | null
}

export function createNativeOfficialPublicationRecovery(
    initialJobIds: readonly string[],
    dependencies: NativeOfficialPublicationRecoveryDependencies,
): NativeOfficialPublicationRecovery {
    const pending = new Set(initialJobIds)
    const listJobIds = dependencies.listJobIds ?? listNativeOfficialPublicationJobs
    const resumeJob = dependencies.resumeJob ?? resumeNativeOfficialPublication
    const recoveredPublications = new Map<string, OfficialRecoveredPublication>()
    let inFlight: Promise<void> | null = null
    const recoveredKey = (accountId: string, revision: number) => `${accountId}\0${revision}`

    const reconcileOnce = async () => {
        for (const jobId of await listJobIds()) pending.add(jobId)
        for (const jobId of [...pending]) {
            const receipt = await resumeJob(jobId)
            if (!receipt) {
                pending.delete(jobId)
                continue
            }
            const publication = receipt.result.publication
            const activeAccountId = dependencies.activeAccountId()
            const authenticationOutcome = publication.kind === 'auth-warning'
                || publication.kind === 'reauthentication-needed'
            if (!activeAccountId || publication.accountId !== activeAccountId) {
                if (authenticationOutcome) {
                    await receipt.acknowledge()
                    pending.delete(jobId)
                }
                continue
            }
            if (authenticationOutcome) {
                dependencies.account.adoptRecoveredOfficialWrite({
                    session: publication.session,
                    warning: publication.warning,
                })
                await receipt.acknowledge()
                pending.delete(jobId)
                continue
            }

            const completion = dependencies.account.adoptRecoveredOfficialWrite({
                session: publication.session,
                warning: publication.kind === 'written' ? publication.warning : null,
                reloadSession: publication.kind === 'written'
                    ? publication.reloadSession
                    : false,
            })
            await dependencies.adapter.adoptPublishedRevision({
                accountId: publication.accountId,
                revision: receipt.result.revision,
                databaseFingerprint: receipt.result.sourceSha256,
            })
            await dependencies.flushMetadata()
            await receipt.acknowledge()
            pending.delete(jobId)
            const recovered = {
                accountId: publication.accountId,
                revision: receipt.result.revision,
                databaseFingerprint: receipt.result.sourceSha256,
            }
            recoveredPublications.set(
                recoveredKey(recovered.accountId, recovered.revision),
                recovered,
            )
            await completion.completeReload()
        }
    }

    return {
        reconcile() {
            if (inFlight) return inFlight
            inFlight = reconcileOnce().finally(() => {
                inFlight = null
            })
            return inFlight
        },
        hasPending: () => pending.size > 0,
        takeRecoveredPublication(accountId, revision) {
            const key = recoveredKey(accountId, revision)
            const recovered = recoveredPublications.get(key) ?? null
            recoveredPublications.delete(key)
            return recovered
        },
    }
}
