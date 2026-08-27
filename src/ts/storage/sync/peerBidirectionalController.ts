import {
    createPeerBidirectionalFacade,
    type PeerBidirectionalCapabilities,
    type PeerBidirectionalDurableOperation,
    type PeerBidirectionalFacade,
    type PeerBidirectionalMutationRuntime,
    type PeerBidirectionalSourceStatus,
    type PeerBidirectionalSyncResult,
} from './peerBidirectional'

export type PeerBidirectionalOperationPhase =
    | 'idle'
    | 'running'
    | 'awaitingConflict'
    | 'localCommitted'
    | 'completed'
    | 'stale'
    | 'failed'

export interface PeerBidirectionalControllerSnapshot {
    capabilities?: PeerBidirectionalCapabilities
    sourceStatus: PeerBidirectionalSourceStatus
    sourcePairingUri: string
    operationPhase: PeerBidirectionalOperationPhase
    operationResult?: PeerBidirectionalSyncResult
    operationId?: string
    error: string
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
        case 'stale': return 'stale'
        default: return 'completed'
    }
}

export function createPeerBidirectionalController(options: {
    facade: PeerBidirectionalFacade
    sourcePollMilliseconds?: number
}) {
    const listeners = new Set<(snapshot: PeerBidirectionalControllerSnapshot) => void>()
    let snapshot: PeerBidirectionalControllerSnapshot = {
        sourceStatus: { phase: 'idle', devices: [] },
        sourcePairingUri: '',
        operationPhase: 'idle',
        error: '',
    }
    let initialized = false
    let initialization: Promise<void> | undefined
    let sourceTimer: ReturnType<typeof setInterval> | undefined
    let sourcePolling = false
    let activeOperation: { key: string; promise: Promise<PeerBidirectionalSyncResult> } | undefined

    const publish = (): void => {
        snapshot = { ...snapshot }
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<PeerBidirectionalControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        publish()
    }
    const stopSourcePolling = (): void => {
        if (sourceTimer) clearInterval(sourceTimer)
        sourceTimer = undefined
    }
    const pollSource = async (): Promise<void> => {
        if (sourcePolling) return
        sourcePolling = true
        try {
            const status = await options.facade.status()
            update({
                sourceStatus: status.source,
                sourcePairingUri: status.source.phase === 'running'
                    ? status.source.pairingUri ?? snapshot.sourcePairingUri
                    : '',
                error: '',
            })
            if (status.source.phase !== 'running') stopSourcePolling()
        } catch (cause) {
            update({ error: cause instanceof Error ? cause.message : String(cause) })
        } finally {
            sourcePolling = false
        }
    }
    const beginSourcePolling = (): void => {
        if (!sourceTimer) {
            sourceTimer = setInterval(
                () => void pollSource(),
                options.sourcePollMilliseconds ?? 1_000,
            )
        }
    }
    const runSource = async <T>(operation: () => Promise<T>): Promise<T> => {
        try {
            const result = await operation()
            update({ error: '' })
            return result
        } catch (cause) {
            update({ error: cause instanceof Error ? cause.message : String(cause) })
            throw cause
        }
    }
    const runOperation = (
        key: string,
        operation: () => Promise<PeerBidirectionalSyncResult>,
    ): Promise<PeerBidirectionalSyncResult> => {
        if (activeOperation) {
            if (activeOperation.key !== key) {
                return Promise.reject(new Error('A peer sync operation for a different pairing is already running'))
            }
            return activeOperation.promise
        }
        update({ operationPhase: 'running', error: '' })
        const promise = operation().then((result) => {
            update({
                operationPhase: resultPhase(result),
                operationResult: result,
                operationId: result.operationId,
                error: '',
            })
            return result
        }).catch((cause) => {
            update({
                operationPhase: 'failed',
                error: cause instanceof Error ? cause.message : String(cause),
            })
            throw cause
        }).finally(() => {
            activeOperation = undefined
        })
        activeOperation = { key, promise }
        return promise
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
            initialization = Promise.all([
                options.facade.capabilities(),
                options.facade.status(),
            ]).then(([capabilities, status]) => {
                update({
                    capabilities,
                    sourceStatus: status.source,
                    sourcePairingUri: status.source.phase === 'running'
                        ? status.source.pairingUri ?? ''
                        : '',
                    ...operationSnapshot(status.operation),
                    error: '',
                })
                if (status.source.phase === 'running') beginSourcePolling()
            }).catch((cause) => {
                initialized = false
                initialization = undefined
                update({ error: cause instanceof Error ? cause.message : String(cause) })
            })
            return initialization
        },
        prepare: () => runSource(async () => {
            const sourceStatus = await options.facade.prepare()
            update({ sourceStatus, sourcePairingUri: '' })
            return sourceStatus
        }),
        start: (sessionId: string) => runSource(async () => {
            const sourceStatus = await options.facade.start(sessionId)
            update({ sourceStatus, sourcePairingUri: sourceStatus.pairingUri ?? '' })
            beginSourcePolling()
            return sourceStatus
        }),
        stop: (sessionId: string) => runSource(async () => {
            await options.facade.stop(sessionId)
            stopSourcePolling()
            const status = await options.facade.status()
            update({ sourceStatus: status.source, sourcePairingUri: '' })
        }),
        revoke: (sessionId: string, deviceId: string) => runSource(async () => {
            await options.facade.revoke(sessionId, deviceId)
            const status = await options.facade.status()
            update({ sourceStatus: status.source })
        }),
        sync(pairingUri: string) {
            return runOperation(`pairing:${pairingUri}`, () => options.facade.sync(pairingUri))
        },
        resolve(winner: 'local' | 'remote') {
            const operationId = snapshot.operationId
            if (!operationId || snapshot.operationPhase !== 'awaitingConflict') {
                return Promise.reject(new Error('No peer sync conflict is awaiting a choice'))
            }
            return runOperation(
                `operation:${operationId}`,
                () => options.facade.resolve(operationId, winner),
            )
        },
        resume() {
            const operationId = snapshot.operationId
            if (!operationId || snapshot.operationPhase !== 'localCommitted') {
                return Promise.reject(new Error('No peer sync operation can be resumed'))
            }
            return runOperation(
                `operation:${operationId}`,
                () => options.facade.resume(operationId),
            )
        },
        async acknowledge(): Promise<void> {
            if (!snapshot.operationId || snapshot.operationPhase !== 'completed') return
            await options.facade.acknowledge(snapshot.operationId)
            update({ operationPhase: 'idle', operationResult: undefined, operationId: undefined })
        },
    }
}

let desktopPeerBidirectionalController: ReturnType<typeof createPeerBidirectionalController> | undefined

export function getDesktopPeerBidirectionalController(runtime: PeerBidirectionalMutationRuntime) {
    desktopPeerBidirectionalController ??= createPeerBidirectionalController({
        facade: createPeerBidirectionalFacade({ platform: 'desktop', runtime }),
    })
    return desktopPeerBidirectionalController
}
