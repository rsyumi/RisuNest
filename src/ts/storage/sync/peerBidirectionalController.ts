import {
    createPeerBidirectionalFacade,
    PeerBidirectionalRefreshError,
    type PeerBidirectionalCapabilities,
    type PeerBidirectionalDurableOperation,
    type PeerBidirectionalFacade,
    type PeerBidirectionalMutationRuntime,
    type PeerBidirectionalSourceStatus,
    type PeerBidirectionalSyncResult,
} from './peerBidirectional'
import { createPeerSourcePolling } from './peerSourcePolling'

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
    sourceStatus: PeerBidirectionalSourceStatus
    sourcePairingUri: string
    operationPhase: PeerBidirectionalOperationPhase
    operationResult?: PeerBidirectionalSyncResult
    operationId?: string
    operationRetained: boolean
    sourceBusy: boolean
    sourceError: string
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

export function createPeerBidirectionalController(options: {
    facade: PeerBidirectionalFacade
    sourcePollMilliseconds?: number
}) {
    const listeners = new Set<(snapshot: PeerBidirectionalControllerSnapshot) => void>()
    let snapshot: PeerBidirectionalControllerSnapshot = {
        sourceStatus: { phase: 'idle', devices: [] },
        sourcePairingUri: '',
        operationPhase: 'idle',
        operationRetained: false,
        sourceBusy: false,
        sourceError: '',
        operationError: '',
    }
    let initialized = false
    let initialization: Promise<void> | undefined
    let sourcePollEpoch = 0
    let operationErrorOwner = 0
    let sourceRefreshErrorOwner: number | undefined
    let activeSourceAction: { key: string; promise: Promise<unknown> } | undefined
    let activeOperation: { key: string; promise: Promise<PeerBidirectionalSyncResult> } | undefined
    let refreshRetry: {
        key: string
        operation: () => Promise<PeerBidirectionalSyncResult>
    } | undefined

    const retainedPhase = (phase: PeerBidirectionalOperationPhase): boolean => [
        'running',
        'awaitingConflict',
        'sourcePrepared',
        'targetPrepared',
        'localCommitted',
        'sourceUnavailable',
        'refreshPending',
        'completed',
    ].includes(phase)

    const publish = (): void => {
        snapshot = { ...snapshot }
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<PeerBidirectionalControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        snapshot.operationRetained = retainedPhase(snapshot.operationPhase)
        publish()
    }
    const sourcePolling = createPeerSourcePolling({
        intervalMilliseconds: options.sourcePollMilliseconds ?? 1_000,
        poll: async (): Promise<void> => {
        const pollEpoch = sourcePollEpoch
        try {
            const status = await options.facade.status()
            if (pollEpoch !== sourcePollEpoch) return
            const clearSourceRefreshError = sourceRefreshErrorOwner === operationErrorOwner
            if (clearSourceRefreshError) sourceRefreshErrorOwner = undefined
            update({
                sourceStatus: status.source,
                sourcePairingUri: status.source.phase === 'running'
                    ? status.source.pairingUri ?? snapshot.sourcePairingUri
                    : '',
                sourceError: '',
                operationError: clearSourceRefreshError ? '' : snapshot.operationError,
                ...operationSnapshot(status.operation),
            })
            if (status.source.phase !== 'running') sourcePolling.stop()
        } catch (cause) {
            if (pollEpoch !== sourcePollEpoch) return
            if (cause instanceof PeerBidirectionalRefreshError && cause.status) {
                operationErrorOwner += 1
                sourceRefreshErrorOwner = operationErrorOwner
                update({
                    sourceStatus: cause.status.source,
                    sourcePairingUri: cause.status.source.pairingUri ?? snapshot.sourcePairingUri,
                    ...operationSnapshot(cause.status.operation),
                    operationError: cause.message,
                })
            } else {
                update({ sourceError: cause instanceof Error ? cause.message : String(cause) })
            }
        }
        },
    })
    const beginSourcePolling = (): void => sourcePolling.start()
    const stopSourcePolling = (): void => sourcePolling.stop()
    const runSource = <T>(
        key: string,
        operation: () => Promise<T>,
        allowRetained: boolean | 'rehost' = false,
    ): Promise<T> => {
        if (activeSourceAction) {
            if (activeSourceAction.key === key) {
                return activeSourceAction.promise as Promise<T>
            }
            return Promise.reject(new Error('A different peer sync source action is already running'))
        }
        const retainedAllowed = allowRetained === true
            || (allowRetained === 'rehost'
                && ['completed', 'sourcePrepared'].includes(snapshot.operationPhase))
        if (snapshot.operationRetained && !retainedAllowed) {
            return Promise.reject(new Error('A retained peer sync operation must be resolved first'))
        }
        const promise = Promise.resolve().then(operation).then((result) => {
            update({ sourceError: '' })
            return result
        }).catch((cause) => {
            update({ sourceError: cause instanceof Error ? cause.message : String(cause) })
            throw cause
        }).finally(() => {
            activeSourceAction = undefined
            update({ sourceBusy: false })
        })
        activeSourceAction = { key, promise }
        update({ sourceBusy: true })
        return promise
    }
    const runOperation = (
        key: string,
        operation: () => Promise<PeerBidirectionalSyncResult>,
    ): Promise<PeerBidirectionalSyncResult> => {
        if (activeOperation) {
            if (activeOperation.key !== key) {
                const differentPairing = activeOperation.key.startsWith('pairing:') && key.startsWith('pairing:')
                return Promise.reject(new Error(differentPairing
                    ? 'A peer sync operation for a different pairing is already running'
                    : 'A different peer sync operation is already running'))
            }
            return activeOperation.promise
        }
        const retained = snapshot.operationRetained ? {
            operationPhase: snapshot.operationPhase,
            operationResult: snapshot.operationResult,
            operationId: snapshot.operationId,
        } : undefined
        operationErrorOwner += 1
        sourceRefreshErrorOwner = undefined
        update({ operationPhase: 'running', operationError: '' })
        const promise = operation().then((result) => {
            refreshRetry = undefined
            operationErrorOwner += 1
            sourceRefreshErrorOwner = undefined
            update({
                operationPhase: resultPhase(result),
                operationResult: result,
                operationId: result.operationId,
                operationError: '',
            })
            return result
        }).catch(async (cause) => {
            operationErrorOwner += 1
            sourceRefreshErrorOwner = undefined
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
                    if (status.operation) {
                        recovered = {
                            sourceStatus: status.source,
                            sourcePairingUri: status.source.phase === 'running'
                                ? status.source.pairingUri ?? snapshot.sourcePairingUri
                                : '',
                            ...operationSnapshot(status.operation),
                        }
                    }
                } catch {
                    // Keep the original operation error and last retained projection.
                }
                update({
                    ...(recovered ?? retained ?? { operationPhase: 'failed' as const }),
                    operationError: cause instanceof Error ? cause.message : String(cause),
                })
            }
            throw cause
        }).finally(() => {
            activeOperation = undefined
        })
        activeOperation = { key, promise }
        return promise
    }
    const startSourceHost = (
        key: string,
        start: () => Promise<PeerBidirectionalSourceStatus>,
    ) => runSource(key, async () => {
        const sourceStatus = await start()
        sourcePollEpoch += 1
        update({ sourceStatus, sourcePairingUri: sourceStatus.pairingUri ?? '' })
        beginSourcePolling()
        return sourceStatus
    }, 'rehost')
    const clearRetainedOperation = async (
        operationId: string,
        refreshStatus = false,
    ): Promise<void> => {
        try {
            await options.facade.acknowledge(operationId)
            const status = refreshStatus ? await options.facade.status() : undefined
            operationErrorOwner += 1
            sourceRefreshErrorOwner = undefined
            update(status
                ? {
                      sourceStatus: status.source,
                      sourcePairingUri: status.source.phase === 'running'
                          ? status.source.pairingUri ?? snapshot.sourcePairingUri
                          : '',
                      ...operationSnapshot(status.operation),
                      operationError: '',
                  }
                : {
                      operationPhase: 'idle',
                      operationResult: undefined,
                      operationId: undefined,
                      operationError: '',
                  })
        } catch (cause) {
            operationErrorOwner += 1
            sourceRefreshErrorOwner = undefined
            update({ operationError: cause instanceof Error ? cause.message : String(cause) })
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
            initialization = (options.facade.recoverTargetForeground?.() ?? Promise.resolve()).then(() => Promise.all([
                options.facade.capabilities(),
                options.facade.status(),
            ])).then(([capabilities, status]) => {
                update({
                    capabilities,
                    sourceStatus: status.source,
                    sourcePairingUri: status.source.phase === 'running'
                        ? status.source.pairingUri ?? ''
                        : '',
                    ...operationSnapshot(status.operation),
                    sourceError: '',
                    operationError: '',
                })
                if (status.source.phase === 'running') beginSourcePolling()
            }).catch((cause) => {
                initialized = false
                initialization = undefined
                update({ sourceError: cause instanceof Error ? cause.message : String(cause) })
            })
            return initialization
        },
        prepare: () => runSource('prepare', async () => {
            const sourceStatus = await options.facade.prepare()
            sourcePollEpoch += 1
            update({ sourceStatus, sourcePairingUri: '' })
            return sourceStatus
        }, 'rehost'),
        start: (sessionId: string) => startSourceHost(`start:${sessionId}`, () => options.facade.start(sessionId)),
        startQuickTunnel: (sessionId: string) => startSourceHost(
            `start-quick:${sessionId}`,
            () => options.facade.startQuickTunnel(sessionId),
        ),
        startNamedTunnel: (
            sessionId: string,
            token: string,
            expectedPublicBaseUrl: string,
        ) => startSourceHost(
            `start-named:${sessionId}`,
            () => options.facade.startNamedTunnel(sessionId, token, expectedPublicBaseUrl),
        ),
        stop: (sessionId: string) => runSource(`stop:${sessionId}`, async () => {
            await options.facade.stop(sessionId)
            stopSourcePolling()
            sourcePollEpoch += 1
            const status = await options.facade.status()
            update({
                sourceStatus: status.source,
                sourcePairingUri: '',
                ...operationSnapshot(status.operation),
            })
        }, true),
        revoke: (sessionId: string, deviceId: string) => runSource(
            `revoke:${sessionId}:${deviceId}`,
            async () => {
                await options.facade.revoke(sessionId, deviceId)
                const status = await options.facade.status()
                sourcePollEpoch += 1
                update({ sourceStatus: status.source })
            },
        ),
        sync(pairingUri: string) {
            const key = `pairing:${pairingUri}`
            if (activeOperation) {
                return runOperation(key, () => options.facade.sync(pairingUri))
            }
            if (
                snapshot.operationRetained
                && ![
                    'awaitingConflict',
                    'targetPrepared',
                    'localCommitted',
                    'sourceUnavailable',
                ].includes(snapshot.operationPhase)
            ) {
                return Promise.reject(new Error('A retained peer sync operation must be resolved first'))
            }
            if (['prepared', 'running'].includes(snapshot.sourceStatus.phase)) {
                return Promise.reject(new Error('A peer sync source is active'))
            }
            return runOperation(key, () => options.facade.sync(pairingUri))
        },
        resolve(winner: 'local' | 'remote', pairingUri?: string) {
            const operationId = snapshot.operationId
            const key = `operation:${operationId ?? ''}:resolve:${winner}:${pairingUri ?? 'retained'}`
            const resolve = (id: string) => pairingUri
                ? options.facade.resolve(id, winner, pairingUri)
                : options.facade.resolve(id, winner)
            if (activeOperation) {
                return runOperation(key, () => resolve(operationId ?? ''))
            }
            if (!operationId || snapshot.operationPhase !== 'awaitingConflict') {
                return Promise.reject(new Error('No peer sync conflict is awaiting a choice'))
            }
            return runOperation(
                key,
                () => resolve(operationId),
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
            return runOperation(
                key,
                () => options.facade.resume(operationId),
            )
        },
        async acknowledge(): Promise<void> {
            if (
                !snapshot.operationId
                || snapshot.operationPhase !== 'completed'
                || ['prepared', 'running'].includes(snapshot.sourceStatus.phase)
            ) return
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
                || ['prepared', 'running'].includes(snapshot.sourceStatus.phase)
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
