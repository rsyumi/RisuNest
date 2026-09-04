import { invoke } from '@tauri-apps/api/core'

import type { PeerClonePlatform } from './peerClone'
import type { PeerSyncForegroundBridge, PeerSyncInvoke, PeerSyncMutationRuntime } from './peerSyncShared'

export type PeerBidirectionalMutationRuntime = PeerSyncMutationRuntime

export interface PeerBidirectionalCapabilities {
    desktop: boolean
    sourceReady: boolean
    atomicActivationReady: boolean
    authenticatedTransportReady: boolean
    losslessBackupReady: boolean
    durableStateReady: boolean
    productionEnabled: boolean
}

export type PeerBidirectionalForegroundBridge = PeerSyncForegroundBridge

interface PeerBidirectionalForegroundIdentity {
    lane: 'p5-target'
    operationId: string
    generation: number
}

interface PeerBidirectionalTargetForegroundStatus {
    foreground: PeerBidirectionalForegroundIdentity
    phase: 'reserved' | 'running' | 'terminal'
    result?: PeerBidirectionalSyncResult
    error?: string
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
    operation?: PeerBidirectionalDurableOperation
}

export type PeerBidirectionalInvoke = PeerSyncInvoke

export interface PeerBidirectionalFacade {
    recoverTargetForeground?(): Promise<void>
    capabilities(): Promise<PeerBidirectionalCapabilities>
    status(): Promise<PeerBidirectionalStatus>
    syncRegistered(deviceId: string): Promise<PeerBidirectionalSyncResult>
    resolveRegistered(
        deviceId: string,
        operationId: string,
        winner: 'local' | 'remote',
    ): Promise<PeerBidirectionalSyncResult>
    resume(operationId: string): Promise<PeerBidirectionalSyncResult>
    acknowledge(operationId: string): Promise<void>
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

    constructor(result: PeerBidirectionalSyncResult, cause: unknown) {
        super(cause instanceof Error ? cause.message : String(cause))
        this.name = 'PeerBidirectionalRefreshError'
        this.result = result
    }
}

export function createPeerBidirectionalFacade(options: {
    platform: PeerClonePlatform
    invoke?: PeerBidirectionalInvoke
    runtime?: PeerBidirectionalMutationRuntime
    bridge?: PeerBidirectionalForegroundBridge
}): PeerBidirectionalFacade {
    const nativeInvoke = options.invoke ?? invoke
    type MutationFence = Awaited<ReturnType<PeerBidirectionalMutationRuntime['acquireDestructiveReplacementFence']>>
    let pendingRefresh: {
        key: string
        result: PeerBidirectionalSyncResult
        revision: number
        fence: MutationFence
        foreground?: PeerBidirectionalForegroundIdentity
        rejection?: { cause: unknown }
    } | undefined
    let targetMutationActive = false
    const bridge = options.bridge ?? (typeof window === 'undefined' ? undefined : window.RisuPeerCloneBridge)
    const requireNative = (): void => {
        if (options.platform === 'web') unsupported(options.platform)
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
        if (command === 'peer_bidirectional_resolve_registered') {
            return operation.phase === 'localCommitted' || operation.phase === 'completed'
        }
        return command === 'peer_bidirectional_resume' && operation.phase === 'completed'
    }
    const sameForegroundIdentity = (
        left: PeerBidirectionalForegroundIdentity,
        right: PeerBidirectionalForegroundIdentity,
    ): boolean => left.lane === right.lane
        && left.operationId === right.operationId
        && left.generation === right.generation
    const releaseTargetForeground = async (
        foreground: PeerBidirectionalForegroundIdentity,
    ): Promise<void> => {
        if (!bridge?.stopSource(foreground.lane, foreground.operationId, foreground.generation)) {
            throw new Error('Android bidirectional foreground service could not stop')
        }
        const released = await nativeInvoke<boolean>('peer_bidirectional_target_foreground_release', { foreground })
        if (!released) throw new Error('Android bidirectional foreground identity is stale')
    }
    const settleTargetForeground = async (
        expected?: PeerBidirectionalForegroundIdentity,
    ): Promise<PeerBidirectionalTargetForegroundStatus | null> => {
        let pending = await nativeInvoke<PeerBidirectionalTargetForegroundStatus | null>(
            'peer_bidirectional_target_foreground_status',
        )
        if (pending && expected && !sameForegroundIdentity(pending.foreground, expected)) {
            throw new Error('Android bidirectional foreground identity is stale')
        }
        if (!pending || pending.phase !== 'running') return pending
        const cancelled = await nativeInvoke<boolean>('peer_bidirectional_target_foreground_cancel', {
            foreground: pending.foreground,
        })
        if (!cancelled) throw new Error('Android bidirectional foreground identity is stale')
        const exact = pending.foreground
        for (let attempt = 0; attempt < 500 && pending?.phase === 'running'; attempt += 1) {
            await new Promise<void>((resolve) => setTimeout(resolve, 10))
            pending = await nativeInvoke<PeerBidirectionalTargetForegroundStatus | null>(
                'peer_bidirectional_target_foreground_status',
            )
            if (pending && !sameForegroundIdentity(pending.foreground, exact)) {
                throw new Error('Android bidirectional foreground identity is stale')
            }
        }
        if (pending?.phase === 'running') {
            throw new Error('Android bidirectional foreground cancellation timed out')
        }
        return pending
    }
    const refreshRecoveredTarget = async (
        result: PeerBidirectionalSyncResult,
    ): Promise<void> => {
        const revision = committedRevision(result)
        if (revision === undefined) return
        const runtime = options.runtime
        if (!runtime) throw new Error('Peer sync mutation runtime is unavailable')
        await runtime.flushPendingData('peer-bidirectional-target-recovery')
        const token = await runtime.capturePersistentMutationToken('peer-bidirectional-target-recovery')
        const fence = await runtime.acquireDestructiveReplacementFence(token)
        try {
            await fence.refreshCommittedWorkingSet(revision)
        } finally {
            fence.release()
        }
    }
    const recoverTargetForeground = async (): Promise<void> => {
        if (options.platform !== 'android') return
        const pending = await settleTargetForeground()
        if (!pending) return
        if (pending.result) await refreshRecoveredTarget(pending.result)
        await releaseTargetForeground(pending.foreground)
    }
    const refreshTargetResult = async (
        key: string,
        result: PeerBidirectionalSyncResult,
        revision: number,
        fence: MutationFence,
        foreground?: PeerBidirectionalForegroundIdentity,
        rejection?: { cause: unknown },
    ): Promise<void> => {
        pendingRefresh = { key, result, revision, fence, foreground, rejection }
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
        requireNative()
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
            if (pending.foreground) await releaseTargetForeground(pending.foreground)
            if (pending.rejection) throw pending.rejection.cause
            return pending.result
        }
        if (options.platform === 'android') await recoverTargetForeground()
        targetMutationActive = true
        let fence: MutationFence | undefined
        let foreground: PeerBidirectionalForegroundIdentity | undefined
        let foregroundReleased = false
        let foregroundResultRefreshed = false
        let foregroundCleanupAttempted = false
        let recoveredResponseError: unknown
        try {
            await runtime.flushPendingData(reason)
            const token = await runtime.capturePersistentMutationToken(reason)
            fence = await runtime.acquireDestructiveReplacementFence(token)
            if (options.platform === 'android') {
                if (!bridge) throw new Error('Android bidirectional foreground service is unavailable')
                foreground = await nativeInvoke<PeerBidirectionalForegroundIdentity>(
                    'peer_bidirectional_target_reserve',
                )
                if (!bridge.startSource(foreground.lane, foreground.operationId, foreground.generation)) {
                    const released = await nativeInvoke<boolean>(
                        'peer_bidirectional_target_foreground_release',
                        { foreground },
                    )
                    if (released) foreground = undefined
                    throw new Error('Android bidirectional foreground service could not start')
                }
            }
            let result: PeerBidirectionalSyncResult | undefined
            try {
                result = await nativeInvoke<PeerBidirectionalSyncResult>(command, {
                    ...args,
                    expectedRevision: token.revision,
                    ...(foreground ? { foreground } : {}),
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
                            try {
                                await refreshTargetResult(
                                    key,
                                    recovered,
                                    revision,
                                    fence,
                                    foreground,
                                    advances ? undefined : { cause },
                                )
                                foregroundResultRefreshed = true
                            } catch (refreshError) {
                                if (refreshError === cause) foregroundResultRefreshed = true
                                throw refreshError
                            }
                        }
                        if (advances) {
                            result = recovered
                            recoveredResponseError = cause
                        }
                    }
                }
                if (!result) throw cause
            }
            if (!result) throw new Error('Peer bidirectional mutation returned no result')
            const revision = committedRevision(result)
            if (revision !== undefined && !foregroundResultRefreshed) {
                await refreshTargetResult(key, result, revision, fence, foreground)
                foregroundResultRefreshed = true
            }
            if (foreground && pendingRefresh?.foreground !== foreground) {
                foregroundCleanupAttempted = true
                try {
                    await releaseTargetForeground(foreground)
                } catch (cleanupError) {
                    if (recoveredResponseError !== undefined) {
                        throw new AggregateError(
                            [recoveredResponseError, cleanupError],
                            'Android bidirectional response recovery and foreground cleanup both failed',
                        )
                    }
                    throw cleanupError
                }
                foregroundReleased = true
            }
            return result
        } catch (error) {
            if (
                !foreground
                || pendingRefresh?.foreground === foreground
                || foregroundCleanupAttempted
            ) throw error
            try {
                const pending = await settleTargetForeground(foreground)
                if (pending?.result && !foregroundResultRefreshed) {
                    await refreshRecoveredTarget(pending.result)
                }
                await releaseTargetForeground(foreground)
                foregroundReleased = true
            } catch (cleanupError) {
                throw new AggregateError(
                    [error, cleanupError],
                    'Android bidirectional operation and foreground cleanup both failed',
                )
            }
            throw error
        } finally {
            targetMutationActive = false
            if (fence && pendingRefresh?.fence !== fence) fence.release()
            void foregroundReleased
        }
    }

    return {
        recoverTargetForeground,
        async capabilities() {
            requireNative()
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
        async status() {
            requireNative()
            return nativeInvoke<PeerBidirectionalStatus>('peer_bidirectional_status')
        },
        syncRegistered(deviceId) {
            return runMutation(
                'peer-bidirectional-sync',
                `registered:${deviceId}`,
                'peer_bidirectional_sync_registered',
                { deviceId },
            )
        },
        resolveRegistered(deviceId, operationId, winner) {
            return runMutation(
                'peer-bidirectional-resolve',
                `registered:${deviceId}:${operationId}:${winner}`,
                'peer_bidirectional_resolve_registered',
                { deviceId, operationId, winner },
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
            requireNative()
            await nativeInvoke('peer_bidirectional_acknowledge', { operationId })
        },
    }
}
