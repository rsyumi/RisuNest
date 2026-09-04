import {
    createPeerBidirectionalFacade,
    PeerBidirectionalRefreshError,
    type PeerBidirectionalCapabilities,
    type PeerBidirectionalDurableOperation,
    type PeerBidirectionalFacade,
    type PeerBidirectionalMutationRuntime,
    type PeerBidirectionalSyncResult,
} from './peerBidirectional'

export type PeerBidirectionalOperationPhase =
    | 'idle'
    | 'running'
    | 'awaitingConflict'
    | 'sourcePrepared'
    | 'targetPrepared'
    | 'localCommitted'
    | 'sourceUnavailable'
    | 'refreshPending'
    | 'completed'
    | 'stale'
    | 'failed'

export interface PeerBidirectionalControllerSnapshot {
    capabilities?: PeerBidirectionalCapabilities
    operationPhase: PeerBidirectionalOperationPhase
    operationResult?: PeerBidirectionalSyncResult
    operationId?: string
    operationRetained: boolean
    operationError: string
}

function operationSnapshot(
    operation: PeerBidirectionalDurableOperation | undefined,
): Partial<PeerBidirectionalControllerSnapshot> {
    if (!operation) return { operationPhase: 'idle', operationResult: undefined, operationId: undefined }
    if (operation.phase === 'awaitingConflict' || operation.phase === 'completed') {
        return {
            operationPhase: operation.phase,
            operationResult: operation.result,
            operationId: operation.result.operationId,
        }
    }
    if (operation.phase === 'sourcePrepared' || operation.phase === 'targetPrepared') {
        return {
            operationPhase: operation.phase,
            operationId: operation.operationId,
            operationResult: undefined,
        }
    }
    return {
        operationPhase: operation.phase,
        operationId: operation.operationId,
        operationResult: {
            kind: 'resumeRequired',
            operationId: operation.operationId,
            phase: operation.phase,
            committedRevision: operation.committedRevision,
        },
    }
}

function resultPhase(result: PeerBidirectionalSyncResult): PeerBidirectionalOperationPhase {
    switch (result.kind) {
        case 'conflict': return 'awaitingConflict'
        case 'resumeRequired': return result.phase
        case 'sourceUnavailable': return 'sourceUnavailable'
        case 'stale': return 'stale'
        default: return 'completed'
    }
}

const retainedPhases: readonly PeerBidirectionalOperationPhase[] = [
    'running',
    'awaitingConflict',
    'sourcePrepared',
    'targetPrepared',
    'localCommitted',
    'sourceUnavailable',
    'refreshPending',
    'completed',
]

export function createPeerBidirectionalController(options: { facade: PeerBidirectionalFacade }) {
    const listeners = new Set<(snapshot: PeerBidirectionalControllerSnapshot) => void>()
    let snapshot: PeerBidirectionalControllerSnapshot = {
        operationPhase: 'idle',
        operationRetained: false,
        operationError: '',
    }
    let initialized = false
    let initialization: Promise<void> | undefined
    let activeOperation: { key: string; promise: Promise<PeerBidirectionalSyncResult> } | undefined
    let refreshRetry: {
        key: string
        operation: () => Promise<PeerBidirectionalSyncResult>
    } | undefined

    const message = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))
    const publish = (): void => {
        snapshot = { ...snapshot }
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<PeerBidirectionalControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        snapshot.operationRetained = retainedPhases.includes(snapshot.operationPhase)
        publish()
    }
    const runOperation = (
        key: string,
        operation: () => Promise<PeerBidirectionalSyncResult>,
    ): Promise<PeerBidirectionalSyncResult> => {
        if (activeOperation) {
            if (activeOperation.key !== key) {
                return Promise.reject(new Error('A different peer sync operation is already running'))
            }
            return activeOperation.promise
        }
        const retained = snapshot.operationRetained ? {
            operationPhase: snapshot.operationPhase,
            operationResult: snapshot.operationResult,
            operationId: snapshot.operationId,
        } : undefined
        update({ operationPhase: 'running', operationError: '' })
        const promise = operation().then((result) => {
            refreshRetry = undefined
            update({
                operationPhase: resultPhase(result),
                operationResult: result,
                operationId: result.operationId,
                operationError: '',
            })
            return result
        }).catch(async (cause) => {
            if (cause instanceof PeerBidirectionalRefreshError) {
                refreshRetry = { key, operation }
                update({
                    operationPhase: 'refreshPending',
                    operationResult: cause.result,
                    operationId: cause.result.operationId,
                    operationError: cause.message,
                })
            } else {
                let recovered: Partial<PeerBidirectionalControllerSnapshot> | undefined
                try {
                    const status = await options.facade.status()
                    if (status.operation) recovered = operationSnapshot(status.operation)
                } catch {
                    // Keep the original operation error and last retained projection.
                }
                update({
                    ...(recovered ?? retained ?? { operationPhase: 'failed' as const }),
                    operationError: message(cause),
                })
            }
            throw cause
        }).finally(() => {
            activeOperation = undefined
        })
        activeOperation = { key, promise }
        return promise
    }
    const clearRetainedOperation = async (
        operationId: string,
        refreshStatus = false,
    ): Promise<void> => {
        try {
            await options.facade.acknowledge(operationId)
            const status = refreshStatus ? await options.facade.status() : undefined
            update(status
                ? { ...operationSnapshot(status.operation), operationError: '' }
                : {
                      operationPhase: 'idle',
                      operationResult: undefined,
                      operationId: undefined,
                      operationError: '',
                  })
        } catch (cause) {
            update({ operationError: message(cause) })
            throw cause
        }
    }

    return {
        snapshot: (): PeerBidirectionalControllerSnapshot => snapshot,
        subscribe(listener: (value: PeerBidirectionalControllerSnapshot) => void): () => void {
            listeners.add(listener)
            listener(snapshot)
            return () => listeners.delete(listener)
        },
        initialize(): Promise<void> {
            if (initialized) return initialization ?? Promise.resolve()
            initialized = true
            initialization = (options.facade.recoverTargetForeground?.() ?? Promise.resolve()).then(
                () => Promise.all([options.facade.capabilities(), options.facade.status()]),
            ).then(([capabilities, status]) => {
                update({
                    capabilities,
                    ...operationSnapshot(status.operation),
                    operationError: '',
                })
            }).catch((cause) => {
                initialized = false
                initialization = undefined
                update({ operationError: message(cause) })
            })
            return initialization
        },
        syncRegistered(deviceId: string) {
            return runOperation(`registered:${deviceId}`, () => options.facade.syncRegistered(deviceId))
        },
        resolveRegistered(deviceId: string, winner: 'local' | 'remote') {
            const operationId = snapshot.operationId
            if (!operationId || snapshot.operationPhase !== 'awaitingConflict') {
                return Promise.reject(new Error('No peer sync conflict is awaiting a choice'))
            }
            return runOperation(
                `registered:${deviceId}:${operationId}:${winner}`,
                () => options.facade.resolveRegistered(deviceId, operationId, winner),
            )
        },
        resume() {
            const operationId = snapshot.operationId
            if (snapshot.operationPhase === 'refreshPending' && refreshRetry) {
                return runOperation(refreshRetry.key, refreshRetry.operation)
            }
            const key = `operation:${operationId ?? ''}:resume`
            if (activeOperation) {
                return runOperation(key, () => options.facade.resume(operationId ?? ''))
            }
            if (
                !operationId
                || !['targetPrepared', 'localCommitted', 'sourceUnavailable'].includes(snapshot.operationPhase)
            ) {
                return Promise.reject(new Error('No peer sync operation can be resumed'))
            }
            return runOperation(key, () => options.facade.resume(operationId))
        },
        async acknowledge(): Promise<void> {
            if (!snapshot.operationId || snapshot.operationPhase !== 'completed') return
            await clearRetainedOperation(snapshot.operationId)
        },
        async abandon(): Promise<void> {
            if (
                !snapshot.operationId
                || ![
                    'awaitingConflict',
                    'sourcePrepared',
                    'targetPrepared',
                    'localCommitted',
                    'sourceUnavailable',
                ].includes(snapshot.operationPhase)
            ) return
            await clearRetainedOperation(
                snapshot.operationId,
                ['sourcePrepared', 'targetPrepared'].includes(snapshot.operationPhase),
            )
        },
    }
}

let desktopPeerBidirectionalController: ReturnType<typeof createPeerBidirectionalController> | undefined

export function getDesktopPeerBidirectionalController(runtime: PeerBidirectionalMutationRuntime) {
    const platform = typeof window !== 'undefined' && window.RisuPeerCloneBridge
        ? 'android' as const
        : 'desktop' as const
    desktopPeerBidirectionalController ??= createPeerBidirectionalController({
        facade: createPeerBidirectionalFacade({ platform, runtime }),
    })
    return desktopPeerBidirectionalController
}
