import { subscribeSyncReceiveCompleted } from './bindingChanges'
import { createNativeSyncBindingBridge } from './bindingNative'
import { getExternalStorageBridge } from './external/bridge'
import { externalJobIsPaused, externalRemedyConnection } from './external/connection'
import { isExternalLwwRunning, requestExternalLwwNow, subscribeExternalLwwCatchUp, subscribeExternalLwwFailures } from './external/lwwProduction'
import { externalProgressFor, externalSyncAttempt, subscribeExternalProgress, type ExternalOperationProgress } from './external/progress'
import { getServerSyncController, retryServerSync } from './serverSyncProduction'
import { installSyncStatus, type SyncStatusSource } from './syncStatus'

type ExternalSource = Extract<SyncStatusSource, { target: 'external' }>

/** External storage pushes no status, so its state is read again when its sync starts or ends, fails, or receives. */
function externalSource(connectionId: string, emit: (value: SyncStatusSource | null) => void): () => void {
    let disposed = false
    let reading = false
    let again = false
    let operation: ExternalOperationProgress | undefined
    let failure: unknown
    let catchingUp = false
    let last: ExternalSource | undefined
    const read = (): void => {
        if (reading) { again = true; return }
        reading = true
        void (async () => {
            try {
                do {
                    again = false
                    const state = await getExternalStorageBridge().getState()
                    if (disposed) return
                    const connection = state.connections.find(item => item.id === connectionId)
                    if (!connection) { last = undefined; emit(null); continue }
                    const job = state.jobs.find(item => item.connectionId === connectionId && (item.state === 'uncertain' || externalJobIsPaused(item)))
                    const selected = state.selection.kind === 'external' && state.selection.connectionId === connectionId
                    const remedy = externalRemedyConnection(failure, connectionId)
                    const remedyConnection = remedy === connectionId ? undefined : state.connections.find(item => item.id === remedy)
                    last = {
                        target: 'external', connection, job, operation, failure, remedyConnection, catchingUp, refresh: read,
                        syncStopped: connection.purpose === 'sync' && selected && !isExternalLwwRunning(connectionId),
                        syncNow: () => requestExternalLwwNow(connectionId),
                    }
                    emit(last)
                } while (again && !disposed)
            } catch (error) {
                console.error('Sync status could not read external storage', error)
            } finally {
                reading = false
            }
        })()
    }
    const stops = [
        subscribeExternalProgress(value => {
            // The button shows every attempt, including a short check the card keeps quiet.
            const next = externalSyncAttempt(value, connectionId) ?? externalProgressFor(value, connectionId)
            if (next?.state === operation?.state && next?.stage === operation?.stage && next?.kind === operation?.kind) return
            const ended = operation?.state === 'running' && next?.state !== 'running'
            operation = next
            if (last) { last = { ...last, operation }; emit(last) }
            if (ended) read()
        }),
        subscribeExternalLwwCatchUp(value => {
            if (value.has(connectionId) === catchingUp) return
            catchingUp = value.has(connectionId)
            if (last) { last = { ...last, catchingUp }; emit(last) }
        }),
        subscribeExternalLwwFailures(value => {
            if (value.get(connectionId) === failure) return
            failure = value.get(connectionId)
            read()
        }),
        subscribeSyncReceiveCompleted(read),
    ]
    read()
    return () => {
        disposed = true
        for (const stop of stops) stop()
    }
}

export function installProductionSyncStatus(): () => void {
    return installSyncStatus({
        target: async () => (await createNativeSyncBindingBridge().state()).target,
        server(emit) {
            const controller = getServerSyncController()
            return controller.subscribe(view => emit({ target: 'server', view, pendingChanges: controller.pendingChanges, syncNow: retryServerSync }))
        },
        external: externalSource,
    })
}
