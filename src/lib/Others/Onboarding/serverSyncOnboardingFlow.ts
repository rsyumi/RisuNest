/**
 * What the sync-server screen of the onboarding shows once a connection has
 * started. The controller snapshot is the only input, so the rules can be
 * checked without a DOM.
 */

import type { ServerSyncSnapshot } from 'src/ts/storage/sync/serverSyncController'
import { completedServerSyncAttempt } from 'src/ts/storage/sync/serverSyncPresenter'

/** The screen before a connection starts: the code box, then the server check. */
export type ServerSyncOnboardingStage = 'code' | 'review' | 'syncing'

export type ServerSyncOnboardingOutcome =
    | 'syncing'
    | 'complete'
    | 'conflict'
    | 'paused'
    | 'pending'
    | 'error'

/**
 * Reads the attempt that the reader started from this screen. `complete`
 * leads to the last onboarding screen; `paused` does too, because the reader
 * stopped it and the settings can continue it. `pending` is a server job
 * that has not finished yet and can simply be retried.
 */
export function serverSyncOnboardingOutcome(
    snapshot: ServerSyncSnapshot,
): ServerSyncOnboardingOutcome {
    if (snapshot.running) return 'syncing'
    if (completedServerSyncAttempt(snapshot) !== null) return 'complete'
    if (snapshot.result?.phase === 'conflict') return 'conflict'
    if (snapshot.paused) return 'paused'
    if (!snapshot.error && snapshot.result?.phase === 'pending') return 'pending'
    return 'error'
}
