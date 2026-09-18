import type { DataRevision } from './persistentDataStore'
import type { CommittedApplyOutcome, PersistentDataRuntime } from './persistentDataRuntime'

type CommittedWorkingSetContinuation = () => void | Promise<void>

let pending:
    | Readonly<{
          minimumRevision: DataRevision
          runtime: object
          authorityEpoch: number
          continueAfterRefresh: CommittedWorkingSetContinuation
          prepareRefresh?: CommittedWorkingSetContinuation
      }>
    | undefined

export function registerCommittedWorkingSetContinuation(
    minimumRevision: DataRevision,
    runtime: object,
    authorityEpoch: number,
    continueAfterRefresh: CommittedWorkingSetContinuation,
    prepareRefresh?: CommittedWorkingSetContinuation,
): void {
    if (pending?.runtime === runtime && pending.authorityEpoch === authorityEpoch) {
        throw new Error('A committed working-set continuation is already pending')
    }
    pending = {
        minimumRevision,
        runtime,
        authorityEpoch,
        continueAfterRefresh,
        prepareRefresh,
    }
}

export async function continueCommittedWorkingSetRefresh(
    revision: DataRevision,
    runtime: object,
    authorityEpoch: number,
): Promise<void> {
    if (!pending) return
    if (pending.runtime !== runtime || pending.authorityEpoch !== authorityEpoch) {
        pending = undefined
        return
    }
    if (revision < pending.minimumRevision) return
    const continuation = pending.continueAfterRefresh
    pending = undefined
    await continuation()
}

async function prepareCommittedWorkingSetRetry(
    runtime: object,
    authorityEpoch: number,
): Promise<boolean> {
    if (!pending) return false
    if (pending.runtime !== runtime || pending.authorityEpoch !== authorityEpoch) {
        pending = undefined
        return false
    }
    const claimed = pending
    await claimed.prepareRefresh?.()
    if (pending === claimed && claimed.prepareRefresh) {
        pending = { ...claimed, prepareRefresh: undefined }
    }
    return true
}

function rebaseCommittedWorkingSetRetry(
    runtime: object,
    previousAuthorityEpoch: number,
    authorityEpoch: number,
): void {
    if (pending?.runtime !== runtime || pending.authorityEpoch !== previousAuthorityEpoch) return
    pending = { ...pending, authorityEpoch }
}

export async function retryCommittedWorkingSetRefreshWithContinuation(
    runtime: Pick<
        PersistentDataRuntime,
        'retryCommittedWorkingSetRefresh' | 'getStorageAuthorityEpoch'
    >,
    onContinuationError?: (error: unknown) => void,
): Promise<CommittedApplyOutcome | null> {
    const authorityEpoch = runtime.getStorageAuthorityEpoch()
    let ownsContinuation: boolean
    try {
        ownsContinuation = await prepareCommittedWorkingSetRetry(runtime, authorityEpoch)
    } catch (error) {
        try {
            onContinuationError?.(error)
        } catch {}
        throw error
    }
    const outcome = await runtime.retryCommittedWorkingSetRefresh()
    if (!ownsContinuation) return outcome
    if (outcome?.projection !== 'applied') {
        rebaseCommittedWorkingSetRetry(
            runtime,
            authorityEpoch,
            runtime.getStorageAuthorityEpoch(),
        )
        return outcome
    }
    try {
        await continueCommittedWorkingSetRefresh(
            outcome.revision,
            runtime,
            authorityEpoch,
        )
    } catch (error) {
        try {
            onContinuationError?.(error)
        } catch {}
    }
    return outcome
}
