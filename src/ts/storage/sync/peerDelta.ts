import { invoke } from '@tauri-apps/api/core'

import type { PeerClonePlatform } from './peerClone'
import { DeviceSyncError } from './deviceSync'
import type { PeerSyncForegroundBridge, PeerSyncInvoke, PeerSyncMutationRuntime } from './peerSyncShared'

export type PeerDeltaInvoke = PeerSyncInvoke

export type PeerDeltaForegroundBridge = PeerSyncForegroundBridge

interface PeerDeltaForegroundIdentity {
    lane: 'p4-target'
    operationId: string
    generation: number
}

interface PeerDeltaTargetForegroundStatus {
    foreground: PeerDeltaForegroundIdentity
    phase: 'reserved' | 'running' | 'terminal'
    result?: PeerDeltaPullResult
    error?: string
}

export type PeerDeltaMutationRuntime = PeerSyncMutationRuntime

export interface PeerDeltaCapabilities {
    desktop: boolean
    atomicActivationReady: boolean
    authenticatedTransportReady: boolean
    productionEnabled: boolean
}

export type PeerDeltaPullResult =
    | {
          kind: 'noChanges' | 'updated'
          revision: number
          transferredObjects: number
          transferredBytes: number
      }
    | {
          kind: 'fullCloneRequired'
          reason: 'noExactCommonBase'
      }
    | {
          kind: 'conflict'
          reason: 'localAndRemoteChanged' | 'staleRevision'
      }

export type RetainedDeltaWitness = 'committed' | 'uncommitted' | 'ambiguous'

// The delta completion the target still holds after a pull ended without
// resolving it. It carries the other device's name and the transfer size only:
// no credential and no endpoint.
export interface RetainedDeltaCompletion {
    operationId: string
    sourceDeviceId: string
    sourceName: string | null
    witness: RetainedDeltaWitness
    transferredObjects: number
    transferredBytes: number
}

const canonicalDeltaOperationUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const canonicalDeltaSourceUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/
const retainedWitnesses: readonly RetainedDeltaWitness[] = ['committed', 'uncommitted', 'ambiguous']

function safeRetainedCompletion(value: unknown): RetainedDeltaCompletion | null {
    if (value === null || value === undefined) return null
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
        throw new DeviceSyncError('state-unavailable')
    }
    const source = value as Record<string, unknown>
    const allowed = new Set([
        'operationId', 'sourceDeviceId', 'sourceName', 'witness', 'transferredObjects', 'transferredBytes',
    ])
    if (
        Object.keys(source).some((key) => !allowed.has(key))
        || typeof source.operationId !== 'string'
        || !canonicalDeltaOperationUuid.test(source.operationId)
        || typeof source.sourceDeviceId !== 'string'
        || !canonicalDeltaSourceUuid.test(source.sourceDeviceId)
        || (source.sourceName !== null && source.sourceName !== undefined && typeof source.sourceName !== 'string')
        || !retainedWitnesses.includes(source.witness as RetainedDeltaWitness)
        || !Number.isSafeInteger(source.transferredObjects)
        || !Number.isSafeInteger(source.transferredBytes)
        || (source.transferredObjects as number) < 0
        || (source.transferredBytes as number) < 0
    ) throw new DeviceSyncError('state-unavailable')
    return {
        operationId: source.operationId,
        sourceDeviceId: source.sourceDeviceId,
        sourceName: typeof source.sourceName === 'string' ? source.sourceName : null,
        witness: source.witness as RetainedDeltaWitness,
        transferredObjects: source.transferredObjects as number,
        transferredBytes: source.transferredBytes as number,
    }
}

type CommittedPeerDeltaPullResult = Extract<
    PeerDeltaPullResult,
    { kind: 'noChanges' | 'updated' }
>

function unsupported(platform: Exclude<PeerClonePlatform, 'desktop'>): never {
    throw new Error(`Peer delta is unsupported on ${platform}`)
}

export function createPeerDeltaFacade(options: {
    platform: PeerClonePlatform
    invoke?: PeerDeltaInvoke
    runtime?: PeerDeltaMutationRuntime
    bridge?: PeerDeltaForegroundBridge
}) {
    const nativeInvoke = options.invoke ?? invoke
    let pendingRefresh: {
        key: string
        result: CommittedPeerDeltaPullResult
        fence: Awaited<ReturnType<PeerDeltaMutationRuntime['acquireDestructiveReplacementFence']>>
        foreground?: PeerDeltaForegroundIdentity
    } | undefined
    const requireNative = (): void => {
        if (options.platform === 'web') unsupported(options.platform)
    }
    const bridge = options.bridge ?? (typeof window === 'undefined' ? undefined : window.RisuPeerCloneBridge)
    const releaseAndroidTargetForeground = async (
        foreground: PeerDeltaForegroundIdentity,
    ): Promise<void> => {
        if (!bridge?.stopSource(foreground.lane, foreground.operationId, foreground.generation)) {
            throw new Error('Android peer delta foreground service could not stop')
        }
        const released = await nativeInvoke<boolean>('peer_delta_target_foreground_release', { foreground })
        if (!released) throw new Error('Android peer delta foreground identity is stale')
    }
    const sameForegroundIdentity = (
        left: PeerDeltaForegroundIdentity,
        right: PeerDeltaForegroundIdentity,
    ): boolean => left.lane === right.lane
        && left.operationId === right.operationId
        && left.generation === right.generation
    const settleTargetForeground = async (
        expected?: PeerDeltaForegroundIdentity,
    ): Promise<PeerDeltaTargetForegroundStatus | null> => {
        let pending = await nativeInvoke<PeerDeltaTargetForegroundStatus | null>(
            'peer_delta_target_foreground_status',
        )
        if (pending && expected && !sameForegroundIdentity(pending.foreground, expected)) {
            throw new Error('Android peer delta foreground identity is stale')
        }
        if (!pending || pending.phase !== 'running') return pending
        const cancelled = await nativeInvoke<boolean>('peer_delta_target_foreground_cancel', {
            foreground: pending.foreground,
        })
        if (!cancelled) throw new Error('Android peer delta foreground identity is stale')
        const exact = pending.foreground
        for (let attempt = 0; attempt < 500 && pending?.phase === 'running'; attempt += 1) {
            await new Promise<void>((resolve) => setTimeout(resolve, 10))
            pending = await nativeInvoke<PeerDeltaTargetForegroundStatus | null>(
                'peer_delta_target_foreground_status',
            )
            if (pending && !sameForegroundIdentity(pending.foreground, exact)) {
                throw new Error('Android peer delta foreground identity is stale')
            }
        }
        if (pending?.phase === 'running') {
            throw new Error('Android peer delta foreground cancellation timed out')
        }
        return pending
    }
    const recoverTargetForeground = async (): Promise<void> => {
        if (options.platform !== 'android') return
        const pending = await settleTargetForeground()
        if (!pending) return
        const runtime = options.runtime
        if (pending.result?.kind === 'updated' || pending.result?.kind === 'noChanges') {
            if (!runtime) throw new Error('Peer delta mutation runtime is unavailable')
            await runtime.flushPendingData('peer-delta-target-recovery')
            const token = await runtime.capturePersistentMutationToken('peer-delta-target-recovery')
            const fence = await runtime.acquireDestructiveReplacementFence(token)
            try {
                await fence.refreshCommittedWorkingSet(pending.result.revision)
            } finally {
                fence.release()
            }
        }
        await releaseAndroidTargetForeground(pending.foreground)
    }
    const runPull = async (
        key: string,
        command: 'peer_delta_pull_registered',
        args: Record<string, unknown>,
    ): Promise<PeerDeltaPullResult> => {
        requireNative()
        const runtime = options.runtime
        if (!runtime) throw new Error('Peer delta mutation runtime is unavailable')
        if (pendingRefresh) {
            const pending = pendingRefresh
            if (pending.key !== key) {
                throw new Error('Another peer delta pull is awaiting renderer refresh')
            }
            await pending.fence.refreshCommittedWorkingSet(pending.result.revision)
            pendingRefresh = undefined
            pending.fence.release()
            if (pending.foreground) await releaseAndroidTargetForeground(pending.foreground)
            return pending.result
        }
        await recoverTargetForeground()
        await runtime.flushPendingData('peer-delta-pull')
        const token = await runtime.capturePersistentMutationToken('peer-delta-pull')
        const fence = await runtime.acquireDestructiveReplacementFence(token)
        let foreground: PeerDeltaForegroundIdentity | undefined
        let fenceReleased = false
        try {
            if (options.platform === 'android') {
                if (!bridge) throw new Error('Android peer delta foreground service is unavailable')
                foreground = await nativeInvoke<PeerDeltaForegroundIdentity>('peer_delta_target_reserve')
                if (!bridge.startSource(foreground.lane, foreground.operationId, foreground.generation)) {
                    const released = await nativeInvoke<boolean>('peer_delta_target_foreground_release', {
                        foreground,
                    })
                    if (released) foreground = undefined
                    throw new Error('Android peer delta foreground service could not start')
                }
            }
            const result = await nativeInvoke<PeerDeltaPullResult>(command, {
                ...args,
                expectedRevision: token.revision,
                ...(foreground ? { foreground } : {}),
            })
            if (result.kind === 'updated' || result.kind === 'noChanges') {
                pendingRefresh = { key, result, fence, foreground }
                await fence.refreshCommittedWorkingSet(result.revision)
                pendingRefresh = undefined
            }
            fence.release()
            fenceReleased = true
            if (foreground) await releaseAndroidTargetForeground(foreground)
            return result
        } catch (error) {
            if (pendingRefresh?.fence === fence) throw error
            if (!foreground) throw error
            try {
                const pending = await settleTargetForeground(foreground)
                if (!fenceReleased) {
                    if (pending?.result?.kind === 'updated' || pending?.result?.kind === 'noChanges') {
                        await fence.refreshCommittedWorkingSet(pending.result.revision)
                    }
                    fence.release()
                    fenceReleased = true
                }
                await releaseAndroidTargetForeground(foreground)
            } catch (cleanupError) {
                if (!fenceReleased) {
                    fence.release()
                    fenceReleased = true
                }
                throw new AggregateError(
                    [error, cleanupError],
                    'Android peer delta pull and foreground cleanup both failed',
                )
            }
            throw error
        } finally {
            if (!fenceReleased && pendingRefresh?.fence !== fence) fence.release()
        }
    }
    const facade = {
        recoverTargetForeground,
        async capabilities(): Promise<PeerDeltaCapabilities> {
            requireNative()
            return nativeInvoke('peer_delta_capabilities')
        },
        async pullRegistered(deviceId: string): Promise<PeerDeltaPullResult> {
            return runPull(`registered:${deviceId}`, 'peer_delta_pull_registered', { deviceId })
        },
        async retained(): Promise<RetainedDeltaCompletion | null> {
            requireNative()
            return safeRetainedCompletion(await nativeInvoke<unknown>('peer_delta_target_retained'))
        },
        async abandonRetained(operationId: string): Promise<void> {
            requireNative()
            await nativeInvoke('peer_delta_target_abandon', { operationId })
        },
    }
    return facade
}
