import { writable } from 'svelte/store'
import type {
    CommittedApplyOutcome,
    PersistentDestructiveReplacementFence,
} from '../../persistentDataRuntime'

export type ExternalApplicationConfirmation =
    | { kind: 'committed'; revision: number }
    | { kind: 'not-applied'; error: unknown }

interface Application {
    jobId: string
    fence: PersistentDestructiveReplacementFence
    confirm(): Promise<ExternalApplicationConfirmation>
    refreshReleased(revision: number): Promise<CommittedApplyOutcome>
    afterRefresh(): Promise<void>
    settled(): void
}

interface PendingApplication {
    application: Application
    fence?: PersistentDestructiveReplacementFence
    revision?: number
    projected: boolean
    running?: Promise<void>
}

let pending: PendingApplication | undefined
const recovery = writable<{ jobId: string; confirmationPending: boolean } | null>(null)
export const externalApplicationRecovery = { subscribe: recovery.subscribe }

export function hasPendingExternalApplication(): boolean {
    return pending !== undefined
}

function settle(current: PendingApplication): void {
    current.fence?.release()
    current.fence = undefined
    if (pending !== current) return
    pending = undefined
    recovery.set(null)
    current.application.settled()
}

async function resume(current: PendingApplication): Promise<void> {
    try {
        if (current.revision === undefined) {
            const confirmation = await current.application.confirm()
            if (confirmation.kind === 'not-applied') {
                settle(current)
                throw confirmation.error
            }
            if (!Number.isSafeInteger(confirmation.revision) || confirmation.revision < 0) {
                throw new Error('External application returned an invalid revision')
            }
            current.revision = confirmation.revision
        }
        if (!current.projected) {
            let outcome: CommittedApplyOutcome
            if (current.fence) {
                outcome = await current.fence.refreshCommittedWorkingSet(current.revision)
                current.fence.release()
                current.fence = undefined
            } else {
                outcome = await current.application.refreshReleased(current.revision)
            }
            if (outcome.projection !== 'applied') {
                throw new Error('Committed external data needs a read-only screen refresh')
            }
            current.projected = true
        }
        await current.application.afterRefresh()
        settle(current)
    } catch (error) {
        if (pending === current) {
            recovery.set({
                jobId: current.application.jobId,
                confirmationPending: current.revision === undefined,
            })
        }
        throw error
    }
}

export async function retryExternalApplication(): Promise<void> {
    const current = pending
    if (!current) return
    if (current.running) return current.running
    current.running = resume(current)
    try {
        await current.running
    } finally {
        current.running = undefined
    }
}

/** One owner retains the original job and fence until its outcome is known. */
export function runExternalApplication(application: Application): Promise<void> {
    if (pending) throw new Error('An external application already needs confirmation')
    pending = { application, fence: application.fence, projected: false }
    return retryExternalApplication()
}
