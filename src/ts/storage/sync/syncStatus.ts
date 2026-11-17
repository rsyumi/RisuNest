import { writable } from 'svelte/store'
import type { BindingTarget } from './bindingFlow'
import { subscribeSyncBindingChanges } from './bindingChanges'
import type { ExternalOperationProgress } from './external/progress'
import type { ExternalConnectionSummary, ExternalJobSummary } from './external/types'
import type { ServerSyncView } from './syncStatusView'

/** What the status button and panel draw from. Labels are made when they render, after the language loads. */
export type SyncStatusSource = { syncNow(): Promise<void> } & (
    | { target: 'server'; view: ServerSyncView; pendingChanges(): Promise<number | undefined> }
    | {
        target: 'external'
        connection: ExternalConnectionSummary
        /** A paused or unfinished job; active jobs are not pushed, so they are left out. */
        job?: ExternalJobSummary
        operation?: ExternalOperationProgress
        /** The last failed sync attempt, until one succeeds. */
        failure?: unknown
        /** Another storage connection whose sign-in or recovery key clears `failure`, which an asset download read through. */
        remedyConnection?: ExternalConnectionSummary
        syncStopped: boolean
        /** The last receive left changes that the next receive continues at once. */
        catchingUp?: boolean
        refresh(): void
    }
)

/** The bound target's state, or null while this device has no sync binding. */
export const syncStatus = writable<SyncStatusSource | null>(null)

export interface SyncStatusSources {
    target(): Promise<BindingTarget>
    server(emit: (value: SyncStatusSource | null) => void): () => void
    external(connectionId: string, emit: (value: SyncStatusSource | null) => void): () => void
}

export function installSyncStatus(sources: SyncStatusSources): () => void {
    let generation = 0
    let stop = () => {}
    const select = async () => {
        const current = ++generation
        const target = await sources.target().catch((): BindingTarget => ({ kind: 'none' }))
        if (current !== generation) return
        stop()
        syncStatus.set(null)
        const emit = (value: SyncStatusSource | null) => { if (current === generation) syncStatus.set(value) }
        stop = target.kind === 'server' ? sources.server(emit)
            : target.kind === 'external' ? sources.external(target.connectionId, emit)
                : () => {}
    }
    const stopBinding = subscribeSyncBindingChanges(() => void select())
    void select()
    return () => {
        generation++
        stopBinding()
        stop()
        syncStatus.set(null)
    }
}
