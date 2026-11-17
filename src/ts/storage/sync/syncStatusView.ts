import { language } from 'src/lang'
import { externalConnectionTitle, externalErrorKind, externalErrorMessage, type ExternalStorageStrings } from 'src/lib/Setting/ExternalStorage/strings'
import { externalJobIsActive, externalJobIsPaused, externalNeedsRenewal, externalNeedsUnlock, externalRemedyConnection, externalRenewalConnection } from './external/connection'
import type { ExternalOperationProgress } from './external/progress'
import type { ExternalConnectionSummary, ExternalJobSummary } from './external/types'
import { formatLevelLabel, registrationCodes, serverSyncErrorMessage } from './serverSyncErrorMessage'
import type { getServerSyncController } from './serverSyncProduction'
import type { SyncStatusSource } from './syncStatus'

export type SyncStatusTone = 'idle' | 'connected' | 'working' | 'paused' | 'failed' | 'attention'
export type ServerSyncView = ReturnType<ReturnType<typeof getServerSyncController>['snapshot']>
export interface SyncStatusView {
    label: string
    tone: SyncStatusTone
    /** Automatic sync stopped until the user acts in settings. */
    stopped: boolean
}

/** The server settings card's badge, shared with the status button so the two never disagree. */
export function serverSyncStatusView(view: ServerSyncView): SyncStatusView {
    const copy = language.risuNest.serverSync
    const connected = !!view.status.configured && !!view.status.bound
    if (view.progress || (connected && view.running)) return { label: copy.running, tone: 'working', stopped: false }
    if (!connected) return { label: copy.disconnected, tone: view.bindingIncomplete ? 'attention' : 'idle', stopped: false }
    if (registrationCodes.includes(view.error)) return { label: copy.registrationRequired, tone: 'attention', stopped: true }
    const format = view.paused ? formatLevelLabel(view.error) : undefined
    if (format) return { label: format, tone: 'attention', stopped: true }
    // An error that is still being retried is not a stop.
    if (view.error) return view.paused ? { label: copy.blocked, tone: 'attention', stopped: true } : { label: copy.failed, tone: 'failed', stopped: false }
    if (view.paused) return { label: copy.paused, tone: 'paused', stopped: false }
    return { label: copy.ready, tone: 'connected', stopped: false }
}

// External storage names the format level stops by error kind; the server codes name their label.
const externalFormatCodes: Record<string, string> = { libraryFormatSwitchRequired: 'library-format-switch-required', appUpdateRequired: 'app-update-required' }
/** The server code of the format level stop a failed external sync attempt reports. */
export const externalFormatCode = (failure: unknown): string | undefined => externalFormatCodes[externalErrorKind(failure) ?? '']

/** The external storage card's badge for one connection, shared with the status button, which passes every running attempt. */
export function externalSyncStatusView(
    strings: ExternalStorageStrings,
    connection: ExternalConnectionSummary,
    job: ExternalJobSummary | undefined,
    operation: ExternalOperationProgress | undefined,
    syncStopped: boolean,
    catchingUp = false,
    failure?: unknown,
): SyncStatusView {
    const usable = connection.status === 'ready' || connection.status === 'paused'
    if (!usable) {
        const label = connection.status === 'reauth-required' ? strings.statusReauth
            : connection.status === 'key-locked' ? strings.statusLocked : strings.statusError
        return { label, tone: 'attention', stopped: true }
    }
    // Sync stops on a sign-in or recovery key failure without marking the connection.
    if (externalNeedsRenewal(failure)) return { label: strings.statusReauth, tone: 'attention', stopped: true }
    if (externalNeedsUnlock(failure)) return { label: strings.statusLocked, tone: 'attention', stopped: true }
    if (job && (job.state === 'uncertain' || externalJobIsPaused(job))) return { label: strings.statusError, tone: 'attention', stopped: false }
    if (operation?.state === 'running') {
        if (operation.stage === 'waiting') return { label: strings.transfer.waiting, tone: 'idle', stopped: false }
        return { label: operation.kind === 'binding' ? strings.transfer.connecting : strings.transfer.syncing, tone: 'working', stopped: false }
    }
    if (job && externalJobIsActive(job)) return { label: strings.jobActive[job.kind], tone: 'working', stopped: false }
    const format = formatLevelLabel(externalFormatCode(failure) ?? '')
    if (format) return { label: format, tone: 'attention', stopped: true }
    // Between receive turns while changes remain, so the badge does not read as caught up.
    if (catchingUp) return { label: strings.transfer.syncing, tone: 'working', stopped: false }
    if (syncStopped) return { label: strings.statusPaused, tone: 'paused', stopped: false }
    return { label: strings.statusReady, tone: 'connected', stopped: false }
}

/** The external connection that has to sign in again, or take a new key, before sync continues: the bound one,
 * or another storage connection an asset download read through. */
export function externalRenewalTarget(source: SyncStatusSource | null | undefined, failure?: unknown): string | undefined {
    if (source?.target !== 'external') return undefined
    const { connection } = source
    if (connection.status === 'reauth-required') return connection.id
    for (const value of [connection.lastError, source.job?.error, source.failure, failure]) {
        const target = externalRenewalConnection(value, connection.id)
        if (target) return target
    }
    return undefined
}

/** The external card's sentence for a connection or job error. */
export function externalErrorLabel(strings: ExternalStorageStrings, value?: ExternalJobSummary['error']): string {
    if (!value) return strings.failed
    if (value.action === 'reauthenticate') return strings.reauthenticate
    if (value.action === 'unlock-key') return strings.unlockKey
    return externalErrorMessage(strings, value)
}

/** The external card's sentence for a failed sync attempt. */
export function externalSyncFailureText(strings: ExternalStorageStrings, failure: unknown): string {
    const kind = externalErrorKind(failure)
    if (kind === 'clockSkew') return language.lwwSync.clockBlocked
    if (kind === 'previousStorageUnavailable') return language.lwwSync.previousStorageUnavailable
    return externalErrorMessage(strings, failure)
}

/** A sign-in or recovery key stop with the connection that clears it, so a closed notice returns when either changes. */
function externalRemedyKey(failure: unknown, connectionId: string): string {
    const remedy = externalRemedyConnection(failure, connectionId)
    return remedy ? `${externalNeedsUnlock(failure) ? 'unlock' : 'renew'}:${remedy}` : ''
}

export interface SyncStatusDisplay extends SyncStatusView {
    /** The settings group title of the bound target. */
    title: string
    /** The sentence the settings card shows for the current error, or empty. */
    error: string
    /** Names the current stop, so the stopped notice returns only when its cause changes. Empty while sync runs. */
    stopKey: string
    lastSuccessAt?: number
}

/** What the status button, panel and stopped notice show. A local save failure takes the badge first. */
export function syncStatusDisplay(source: SyncStatusSource, strings: ExternalStorageStrings, saveFailed: boolean): SyncStatusDisplay {
    let display: SyncStatusDisplay
    if (source.target === 'server') {
        const { view } = source
        const status = serverSyncStatusView(view)
        display = {
            ...status,
            title: language.risuNest.serverSync.title,
            error: view.error ? serverSyncErrorMessage({ code: view.error }) : '',
            stopKey: status.stopped ? `server:${view.error}` : '',
            lastSuccessAt: view.lastSuccessAt,
        }
    } else {
        const { connection, remedyConnection } = source
        const status = externalSyncStatusView(strings, connection, source.job, source.operation, source.syncStopped, source.catchingUp, source.failure)
        display = {
            ...status,
            title: strings.title,
            error: connection.lastError ? externalErrorLabel(strings, connection.lastError)
                : remedyConnection ? (externalNeedsUnlock(source.failure) ? strings.unlockKeyOtherAssets : strings.reauthenticateOtherAssets)
                    .replace('{0}', externalConnectionTitle(strings, remedyConnection))
                : source.failure !== undefined ? externalSyncFailureText(strings, source.failure) : '',
            stopKey: status.stopped ? `external:${connection.id}:${connection.status}:${connection.lastError?.code
                ?? externalFormatCode(source.failure) ?? externalRemedyKey(source.failure, connection.id)}` : '',
            lastSuccessAt: connection.lastSyncAtMs ? Number(connection.lastSyncAtMs) : undefined,
        }
    }
    return saveFailed ? { ...display, label: language.risuNest.syncStatus.saveFailed, tone: 'attention' } : display
}
