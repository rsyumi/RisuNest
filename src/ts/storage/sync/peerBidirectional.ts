import { invoke } from '@tauri-apps/api/core'

import {
    parsePeerLanEndpoint,
    type PeerClonePlatform,
} from './peerClone'

export interface PeerBidirectionalPairing {
    endpoint: string
    sessionId: string
    manifestId: string
    claim: string
}

export interface PeerBidirectionalMutationRuntime {
    flushPendingData(reason: string): Promise<void>
    capturePersistentMutationToken(reason: string): Promise<{
        revision: number
        mutationGeneration: number
    }>
    acquirePersistentMutationFence(token: {
        revision: number
        mutationGeneration: number
    }): Promise<{
        refreshCommittedWorkingSet(revision: number): Promise<void>
        release(): void
    }>
}

export interface PeerBidirectionalCapabilities {
    desktop: true
    sourceReady: boolean
    atomicActivationReady: boolean
    authenticatedTransportReady: boolean
    losslessBackupReady: boolean
    durableStateReady: boolean
    productionEnabled: boolean
}

export interface PeerBidirectionalSourceStatus {
    phase: 'idle' | 'prepared' | 'running' | 'stopped'
    sessionId?: string
    manifestId?: string
    pairingUri?: string
    devices: readonly {
        deviceId: string
        transferredBytes: number
        currentObject?: string
        lastSeenAt: number
        revoked: boolean
    }[]
}

export type PeerBidirectionalConflictType = 'sameRecord' | 'deleteVsEdit'

export interface PeerBidirectionalBackupReceipt {
    packageId: string
    side: 'local' | 'remote'
    path: string
}

export type PeerBidirectionalCompletedResult = {
    kind: 'noChanges' | 'updated'
    operationId: string
    revision: number
    remoteRevision: number
    transferredObjects: number
    transferredBytes: number
    backups: PeerBidirectionalBackupReceipt[]
}

export type PeerBidirectionalSyncResult =
    | PeerBidirectionalCompletedResult
    | {
          kind: 'conflict'
          operationId: string
          conflicts: Array<{ key: string; type: PeerBidirectionalConflictType }>
          localManifestHash: string
          remoteManifestHash: string
      }
    | {
          kind: 'stale'
          operationId: string
          reason: 'localRevision' | 'remoteGeneration' | 'commonBase' | 'deviceAcknowledgement'
      }
    | {
          kind: 'resumeRequired'
          operationId: string
          phase: 'localCommitted'
          committedRevision: number
      }
    | {
          kind: 'sourceUnavailable'
          operationId: string
          committedRevision: number
      }

export type PeerBidirectionalDurableOperation =
    | {
          phase: 'sourcePrepared'
          operationId: string
      }
    | {
          phase: 'targetPrepared'
          operationId: string
      }
    | {
          phase: 'awaitingConflict'
          result: Extract<PeerBidirectionalSyncResult, { kind: 'conflict' }>
      }
    | {
          phase: 'localCommitted'
          operationId: string
          committedRevision: number
      }
    | {
          phase: 'completed'
          result: PeerBidirectionalCompletedResult
      }

export interface PeerBidirectionalStatus {
    source: PeerBidirectionalSourceStatus
    operation?: PeerBidirectionalDurableOperation
}

export interface PeerBidirectionalInvoke {
    <T>(command: string, args?: Record<string, unknown>): Promise<T>
}

export interface PeerBidirectionalFacade {
    capabilities(): Promise<PeerBidirectionalCapabilities>
    prepare(): Promise<PeerBidirectionalSourceStatus>
    start(sessionId: string): Promise<PeerBidirectionalSourceStatus>
    status(): Promise<PeerBidirectionalStatus>
    stop(sessionId: string): Promise<void>
    revoke(sessionId: string, deviceId: string): Promise<void>
    sync(pairingUri: string): Promise<PeerBidirectionalSyncResult>
    resolve(
        operationId: string,
        winner: 'local' | 'remote',
    ): Promise<PeerBidirectionalSyncResult>
    resume(operationId: string): Promise<PeerBidirectionalSyncResult>
    acknowledge(operationId: string): Promise<void>
}

const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const sha256Pattern = /^[0-9a-f]{64}$/
const maximumPairingUriLength = 8192

function invalidPairingUri(): never {
    throw new Error('Invalid peer sync pairing URI')
}

export function parsePeerBidirectionalUri(value: string): PeerBidirectionalPairing {
    if (value.length === 0 || value.length > maximumPairingUriLength) return invalidPairingUri()
    let uri: URL
    try {
        uri = new URL(value)
    } catch {
        return invalidPairingUri()
    }
    if (uri.protocol !== 'risuailocal:' || uri.hostname !== 'peer-sync' || uri.pathname !== '/v1') {
        return invalidPairingUri()
    }
    const expectedKeys = ['endpoint', 'session', 'manifest']
    if (
        [...uri.searchParams.keys()].length !== expectedKeys.length
        || expectedKeys.some((key) => uri.searchParams.getAll(key).length !== 1)
        || [...uri.searchParams.keys()].some((key) => !expectedKeys.includes(key))
    ) return invalidPairingUri()

    const sessionId = uri.searchParams.get('session')!
    const manifestId = uri.searchParams.get('manifest')!
    const fragment = uri.hash.slice(1)
    if (
        !uuidPattern.test(sessionId)
        || !sha256Pattern.test(manifestId)
        || !/^claim=[0-9a-f]{64}$/.test(fragment)
    ) return invalidPairingUri()
    let endpoint: string
    try {
        endpoint = parsePeerLanEndpoint(uri.searchParams.get('endpoint')!)
    } catch {
        return invalidPairingUri()
    }
    return {
        endpoint: endpoint.endsWith('/') ? endpoint.slice(0, -1) : endpoint,
        sessionId,
        manifestId,
        claim: fragment.slice('claim='.length),
    }
}

function unsupported(platform: Exclude<PeerClonePlatform, 'desktop'>): never {
    throw new Error(`Peer sync is unsupported on ${platform}`)
}

function completed(
    result: PeerBidirectionalSyncResult,
): result is PeerBidirectionalCompletedResult {
    return result.kind === 'noChanges' || result.kind === 'updated'
}

export class PeerBidirectionalRefreshError extends Error {
    readonly result: PeerBidirectionalSyncResult
    readonly status?: PeerBidirectionalStatus

    constructor(
        result: PeerBidirectionalSyncResult,
        cause: unknown,
        status?: PeerBidirectionalStatus,
    ) {
        super(cause instanceof Error ? cause.message : String(cause))
        this.name = 'PeerBidirectionalRefreshError'
        this.result = result
        this.status = status
    }
}

export function createPeerBidirectionalFacade(options: {
    platform: PeerClonePlatform
    invoke?: PeerBidirectionalInvoke
    runtime?: PeerBidirectionalMutationRuntime
}): PeerBidirectionalFacade {
    const nativeInvoke = options.invoke ?? invoke
    type MutationFence = Awaited<ReturnType<PeerBidirectionalMutationRuntime['acquirePersistentMutationFence']>>
    let pendingRefresh: {
        key: string
        result: PeerBidirectionalSyncResult
        revision: number
        fence: MutationFence
        rejection?: { cause: unknown }
    } | undefined
    let sourceFence: MutationFence | undefined
    let pendingSourceRefresh: {
        key: string
        revision: number
        status: PeerBidirectionalStatus
    } | undefined
    let sourceRefreshInFlight: {
        key: string
        promise: Promise<PeerBidirectionalStatus>
    } | undefined
    let refreshedSourceOperation = ''
    let sourceRefreshHighWater = -1
    let sourcePrepareActive = false
    let sourceStopCompleted = false
    let sourceStopStatusPending = false
    let targetMutationActive = false
    const requireDesktop = (): void => {
        if (options.platform !== 'desktop') unsupported(options.platform)
    }
    const committedRevision = (result: PeerBidirectionalSyncResult): number | undefined => {
        if (completed(result)) return result.revision
        if (result.kind === 'resumeRequired' || result.kind === 'sourceUnavailable') {
            return result.committedRevision
        }
        return undefined
    }
    const projectOperation = (
        operation: PeerBidirectionalDurableOperation,
    ): PeerBidirectionalSyncResult | undefined => {
        if (operation.phase === 'sourcePrepared' || operation.phase === 'targetPrepared') return undefined
        if (operation.phase === 'awaitingConflict' || operation.phase === 'completed') {
            return operation.result
        }
        return {
            kind: 'resumeRequired',
            operationId: operation.operationId,
            phase: 'localCommitted',
            committedRevision: operation.committedRevision,
        }
    }
    const recoveredOperationAdvances = (
        command: string,
        args: Record<string, unknown>,
        operation: PeerBidirectionalDurableOperation,
    ): boolean => {
        const operationId = typeof args.operationId === 'string' ? args.operationId : undefined
        if (
            !operationId
            || operation.phase === 'awaitingConflict'
            || operation.phase === 'sourcePrepared'
            || operation.phase === 'targetPrepared'
        ) {
            return false
        }
        const recoveredOperationId = operation.phase === 'completed'
            ? operation.result.operationId
            : operation.operationId
        if (recoveredOperationId !== operationId) return false
        if (command === 'peer_bidirectional_resolve') {
            return operation.phase === 'localCommitted' || operation.phase === 'completed'
        }
        return command === 'peer_bidirectional_resume' && operation.phase === 'completed'
    }
    const refreshTargetResult = async (
        key: string,
        result: PeerBidirectionalSyncResult,
        revision: number,
        fence: MutationFence,
        rejection?: { cause: unknown },
    ): Promise<void> => {
        pendingRefresh = { key, result, revision, fence, rejection }
        try {
            await fence.refreshCommittedWorkingSet(revision)
        } catch (cause) {
            throw new PeerBidirectionalRefreshError(result, cause)
        }
        pendingRefresh = undefined
        if (rejection) throw rejection.cause
    }
    const runMutation = async (
        reason: string,
        key: string,
        command: string,
        args: Record<string, unknown>,
    ): Promise<PeerBidirectionalSyncResult> => {
        requireDesktop()
        if (sourceFence || sourcePrepareActive) throw new Error('A peer sync source is active')
        if (targetMutationActive) throw new Error('A peer sync target is active')
        const runtime = options.runtime
        if (!runtime) throw new Error('Peer sync mutation runtime is unavailable')
        if (pendingRefresh) {
            if (pendingRefresh.key !== key) {
                throw new Error('A different peer sync retry is awaiting renderer refresh')
            }
            const pending = pendingRefresh
            try {
                await pending.fence.refreshCommittedWorkingSet(pending.revision)
            } catch (cause) {
                throw new PeerBidirectionalRefreshError(pending.result, cause)
            }
            pendingRefresh = undefined
            pending.fence.release()
            if (pending.rejection) throw pending.rejection.cause
            return pending.result
        }
        targetMutationActive = true
        let fence: MutationFence | undefined
        try {
            await runtime.flushPendingData(reason)
            const token = await runtime.capturePersistentMutationToken(reason)
            fence = await runtime.acquirePersistentMutationFence(token)
            let result: PeerBidirectionalSyncResult
            try {
                result = await nativeInvoke<PeerBidirectionalSyncResult>(command, {
                    ...args,
                    expectedRevision: token.revision,
                })
            } catch (cause) {
                let status: PeerBidirectionalStatus
                try {
                    status = await nativeInvoke<PeerBidirectionalStatus>('peer_bidirectional_status')
                } catch {
                    throw cause
                }
                if (status.operation) {
                    const recovered = projectOperation(status.operation)
                    const advances = recoveredOperationAdvances(command, args, status.operation)
                    if (recovered) {
                        const revision = committedRevision(recovered)
                        if (revision !== undefined) {
                            await refreshTargetResult(
                                key,
                                recovered,
                                revision,
                                fence,
                                advances ? undefined : { cause },
                            )
                        }
                        if (advances) return recovered
                    }
                }
                throw cause
            }
            const revision = committedRevision(result)
            if (revision !== undefined) {
                await refreshTargetResult(key, result, revision, fence)
            }
            return result
        } finally {
            targetMutationActive = false
            if (fence && pendingRefresh?.fence !== fence) fence.release()
        }
    }

    const releaseStoppedSourceFence = (): void => {
        if (!sourceStopCompleted || sourceStopStatusPending || pendingSourceRefresh) return
        const fence = sourceFence
        sourceFence = undefined
        pendingSourceRefresh = undefined
        refreshedSourceOperation = ''
        sourceRefreshHighWater = -1
        sourceStopCompleted = false
        sourceStopStatusPending = false
        fence?.release()
    }

    const refreshSourceStatus = async (
        status: PeerBidirectionalStatus,
        key: string,
        revision: number,
    ): Promise<PeerBidirectionalStatus> => {
        const fence = sourceFence
        if (!fence || refreshedSourceOperation === key) {
            releaseStoppedSourceFence()
            return status
        }
        pendingSourceRefresh = { key, revision, status }
        if (sourceRefreshInFlight) {
            if (sourceRefreshInFlight.key === key) return sourceRefreshInFlight.promise
            const previous = sourceRefreshInFlight
            await previous.promise
            if (sourceRefreshInFlight?.promise === previous.promise) {
                sourceRefreshInFlight = undefined
            }
            return refreshSourceStatus(status, key, revision)
        }
        const promise = (async (): Promise<PeerBidirectionalStatus> => {
            try {
                await fence.refreshCommittedWorkingSet(revision)
            } catch (cause) {
                const operation = status.operation
                if (operation?.phase !== 'completed') throw cause
                throw new PeerBidirectionalRefreshError(operation.result, cause, status)
            }
            refreshedSourceOperation = key
            if (pendingSourceRefresh?.key === key) pendingSourceRefresh = undefined
            releaseStoppedSourceFence()
            return status
        })()
        sourceRefreshInFlight = { key, promise }
        try {
            return await promise
        } finally {
            if (sourceRefreshInFlight?.promise === promise) sourceRefreshInFlight = undefined
        }
    }

    const committedSourceOperation = (
        status: PeerBidirectionalStatus,
    ): { key: string; revision: number } | undefined => {
        if (status.operation?.phase === 'completed') {
            const revision = status.operation.result.revision
            if (sourceFence && revision < sourceRefreshHighWater) return undefined
            if (sourceFence) sourceRefreshHighWater = Math.max(sourceRefreshHighWater, revision)
            return {
                key: `${status.operation.result.operationId}:${revision}`,
                revision,
            }
        }
        return undefined
    }

    return {
        async capabilities() {
            requireDesktop()
            const capabilities = await nativeInvoke<PeerBidirectionalCapabilities>('peer_bidirectional_capabilities')
            return {
                ...capabilities,
                productionEnabled: capabilities.productionEnabled
                    && capabilities.sourceReady
                    && capabilities.atomicActivationReady
                    && capabilities.authenticatedTransportReady
                    && capabilities.losslessBackupReady
                    && capabilities.durableStateReady,
            }
        },
        async prepare() {
            requireDesktop()
            if (sourceFence || sourcePrepareActive || targetMutationActive || pendingRefresh) {
                throw new Error('A peer sync source or target is already active')
            }
            const runtime = options.runtime
            if (!runtime) throw new Error('Peer sync mutation runtime is unavailable')
            sourcePrepareActive = true
            let fence: MutationFence | undefined
            try {
                await runtime.flushPendingData('peer-bidirectional-source-prepare')
                const token = await runtime.capturePersistentMutationToken('peer-bidirectional-source-prepare')
                fence = await runtime.acquirePersistentMutationFence(token)
                const status = await nativeInvoke<PeerBidirectionalSourceStatus>('peer_bidirectional_prepare', {
                    expectedRevision: token.revision,
                })
                sourceFence = fence
                refreshedSourceOperation = ''
                sourceRefreshHighWater = -1
                sourceStopCompleted = false
                sourceStopStatusPending = false
                return status
            } catch (cause) {
                fence?.release()
                throw cause
            } finally {
                sourcePrepareActive = false
            }
        },
        async start(sessionId) {
            requireDesktop()
            if (!sourceFence) throw new Error('Peer sync source is not prepared')
            return nativeInvoke('peer_bidirectional_start', { sessionId })
        },
        async status() {
            requireDesktop()
            if (pendingSourceRefresh && !sourceStopStatusPending) {
                const pending = pendingSourceRefresh
                return refreshSourceStatus(pending.status, pending.key, pending.revision)
            }
            const completesStopStatus = sourceStopStatusPending && sourceStopCompleted
            const status = await nativeInvoke<PeerBidirectionalStatus>('peer_bidirectional_status')
            const committed = committedSourceOperation(status)
            if (committed) {
                const refreshed = refreshSourceStatus(status, committed.key, committed.revision)
                if (completesStopStatus) sourceStopStatusPending = false
                const result = await refreshed
                releaseStoppedSourceFence()
                return result
            }
            if (sourceStopStatusPending && pendingSourceRefresh) {
                const pending = pendingSourceRefresh
                const refreshed = refreshSourceStatus(pending.status, pending.key, pending.revision)
                if (completesStopStatus) sourceStopStatusPending = false
                await refreshed
                releaseStoppedSourceFence()
                return status
            }
            if (completesStopStatus) sourceStopStatusPending = false
            releaseStoppedSourceFence()
            return status
        },
        async stop(sessionId) {
            requireDesktop()
            sourceStopStatusPending = true
            if (!sourceStopCompleted) {
                try {
                    await nativeInvoke('peer_bidirectional_stop', { sessionId })
                } catch (cause) {
                    sourceStopStatusPending = false
                    throw cause
                }
                sourceStopCompleted = true
            }
            const status = await nativeInvoke<PeerBidirectionalStatus>('peer_bidirectional_status')
            const committed = committedSourceOperation(status)
            if (committed) {
                const refreshed = refreshSourceStatus(status, committed.key, committed.revision)
                sourceStopStatusPending = false
                await refreshed
                releaseStoppedSourceFence()
            } else if (pendingSourceRefresh) {
                const pending = pendingSourceRefresh
                const refreshed = refreshSourceStatus(pending.status, pending.key, pending.revision)
                sourceStopStatusPending = false
                await refreshed
                releaseStoppedSourceFence()
            } else {
                sourceStopStatusPending = false
                releaseStoppedSourceFence()
            }
        },
        async revoke(sessionId, deviceId) {
            requireDesktop()
            await nativeInvoke('peer_bidirectional_revoke', { sessionId, deviceId })
        },
        sync(pairingUri) {
            const pairing = parsePeerBidirectionalUri(pairingUri)
            return runMutation(
                'peer-bidirectional-sync',
                `pairing:${pairingUri}`,
                'peer_bidirectional_sync',
                { ...pairing },
            )
        },
        resolve(operationId, winner) {
            return runMutation(
                'peer-bidirectional-resolve',
                `operation:${operationId}:resolve:${winner}`,
                'peer_bidirectional_resolve',
                { operationId, winner },
            )
        },
        resume(operationId) {
            return runMutation(
                'peer-bidirectional-resume',
                `operation:${operationId}:resume`,
                'peer_bidirectional_resume',
                { operationId },
            )
        },
        async acknowledge(operationId) {
            requireDesktop()
            await nativeInvoke('peer_bidirectional_acknowledge', { operationId })
        },
    }
}
