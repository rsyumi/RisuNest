import { invoke } from '@tauri-apps/api/core'

import {
    parsePeerPairingUri,
    type PeerClonePlatform,
    type PeerCloneTunnelMetadata,
    type PeerCloneTunnelStatus,
} from './peerClone'
import type { PeerSyncForegroundBridge, PeerSyncInvoke, PeerSyncMutationRuntime } from './peerSyncShared'

export interface PeerDeltaPairing {
    endpoint: string
    sessionId: string
    manifestId: string
    claim: string
}

export type PeerDeltaInvoke = PeerSyncInvoke

export type PeerDeltaForegroundBridge = PeerSyncForegroundBridge

interface PeerDeltaForegroundIdentity {
    lane: 'p4-source' | 'p4-target'
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
    sourceReady: boolean
    atomicActivationReady: boolean
    authenticatedTransportReady: boolean
    productionEnabled: boolean
    tunnelReady: boolean
}

export interface PeerDeltaSourceStatus {
    sessionId?: string
    manifestId?: string
    pairingUri?: string
    phase: 'idle' | 'prepared' | 'starting' | 'running' | 'stopping' | 'stopped'
    tunnel?: PeerCloneTunnelMetadata
    devices: readonly {
        deviceId: string
        transferredBytes: number
        currentObject?: string
        lastSeenAt: number
        revoked: boolean
    }[]
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

type CommittedPeerDeltaPullResult = Extract<
    PeerDeltaPullResult,
    { kind: 'noChanges' | 'updated' }
>

function invalidPairingUri(): never {
    throw new Error('Invalid peer delta pairing URI')
}

export function parsePeerDeltaUri(value: string): PeerDeltaPairing {
    return parsePeerPairingUri(value, {
        hostname: 'peer-delta',
        invalid: invalidPairingUri,
        claimRule: 'hex64Fragment',
        allowLanEndpoint: true,
        trimTrailingSlash: false,
    })
}

function unsupported(platform: Exclude<PeerClonePlatform, 'desktop'>): never {
    throw new Error(`Peer delta is unsupported on ${platform}`)
}

function samePairing(left: PeerDeltaPairing, right: PeerDeltaPairing): boolean {
    return left.endpoint === right.endpoint
        && left.sessionId === right.sessionId
        && left.manifestId === right.manifestId
        && left.claim === right.claim
}

export function createPeerDeltaFacade(options: {
    platform: PeerClonePlatform
    invoke?: PeerDeltaInvoke
    runtime?: PeerDeltaMutationRuntime
    bridge?: PeerDeltaForegroundBridge
}) {
    const nativeInvoke = options.invoke ?? invoke
    let pendingRefresh: {
        pairing: PeerDeltaPairing
        result: CommittedPeerDeltaPullResult
        fence: Awaited<ReturnType<PeerDeltaMutationRuntime['acquireDestructiveReplacementFence']>>
        foreground?: PeerDeltaForegroundIdentity
    } | undefined
    const requireDesktop = (): void => {
        if (options.platform !== 'desktop') unsupported(options.platform)
    }
    const requireNative = (): void => {
        if (options.platform === 'web') unsupported(options.platform)
    }
    const bridge = options.bridge ?? (typeof window === 'undefined' ? undefined : window.RisuPeerCloneBridge)
    const abandonSourceReservation = async (foreground: PeerDeltaForegroundIdentity): Promise<void> => {
        const abandoned = await nativeInvoke<boolean>('peer_sync_foreground_source_abandon', { foreground })
        if (!abandoned) throw new Error('Android peer delta source foreground identity is stale')
    }
    const stopAndAbandonSourceReservation = async (
        foreground: PeerDeltaForegroundIdentity,
    ): Promise<void> => {
        if (!bridge?.stopSource(foreground.lane, foreground.operationId, foreground.generation)) {
            throw new Error('Android peer delta source foreground service could not stop')
        }
        await abandonSourceReservation(foreground)
    }
    const recoverSourceReservation = async (): Promise<void> => {
        const pending = await nativeInvoke<PeerDeltaForegroundIdentity | null>(
            'peer_sync_foreground_source_status',
            { lane: 'p4-source' },
        )
        if (pending?.lane === 'p4-source') await stopAndAbandonSourceReservation(pending)
    }
    const failAfterCleanup = async (
        error: unknown,
        cleanup: () => Promise<void>,
        message: string,
    ): Promise<never> => {
        try {
            await cleanup()
        } catch (cleanupError) {
            throw new AggregateError([error, cleanupError], message)
        }
        throw error
    }
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
    const facade = {
        recoverTargetForeground,
        async capabilities(): Promise<PeerDeltaCapabilities> {
            requireNative()
            return nativeInvoke('peer_delta_capabilities')
        },
        async prepare(): Promise<PeerDeltaSourceStatus> {
            requireNative()
            const runtime = options.runtime
            if (!runtime) throw new Error('Peer delta mutation runtime is unavailable')
            await runtime.flushPendingData('peer-delta-source-prepare')
            return nativeInvoke('peer_delta_prepare')
        },
        async start(sessionId: string): Promise<PeerDeltaSourceStatus> {
            requireNative()
            if (options.platform === 'desktop') return nativeInvoke('peer_delta_start', { sessionId })
            if (!bridge) throw new Error('Android peer delta foreground service is unavailable')
            await recoverSourceReservation()
            const foreground = await nativeInvoke<PeerDeltaForegroundIdentity>('peer_delta_source_reserve')
            let started: boolean
            try {
                started = bridge.startSource(foreground.lane, foreground.operationId, foreground.generation)
            } catch (error) {
                return failAfterCleanup(
                    error,
                    () => stopAndAbandonSourceReservation(foreground),
                    'Android peer delta foreground start and cleanup both failed',
                )
            }
            if (!started) {
                return failAfterCleanup(
                    new Error('Android peer delta foreground service could not start'),
                    () => abandonSourceReservation(foreground),
                    'Android peer delta foreground start and cleanup both failed',
                )
            }
            try {
                return await nativeInvoke('peer_delta_start', { sessionId, foreground })
            } catch (error) {
                return failAfterCleanup(
                    error,
                    () => stopAndAbandonSourceReservation(foreground),
                    'Android peer delta foreground start and cleanup both failed',
                )
            }
        },
        async startQuickTunnel(sessionId: string): Promise<PeerDeltaSourceStatus> {
            requireDesktop()
            return nativeInvoke('peer_delta_tunnel_start', {
                sessionId,
                tunnel: { kind: 'quick' },
            })
        },
        async startNamedTunnel(
            sessionId: string,
            token: string,
            expectedPublicBaseUrl: string,
        ): Promise<PeerDeltaSourceStatus> {
            requireDesktop()
            return nativeInvoke('peer_delta_tunnel_start', {
                sessionId,
                tunnel: { kind: 'named', token, expectedPublicBaseUrl },
            })
        },
        async tunnelStatus(): Promise<PeerCloneTunnelStatus> {
            requireDesktop()
            return nativeInvoke('peer_delta_tunnel_status')
        },
        async stopTunnel(sessionId: string): Promise<void> {
            requireDesktop()
            await nativeInvoke('peer_delta_tunnel_stop', { sessionId })
        },
        async status(): Promise<PeerDeltaSourceStatus> {
            requireNative()
            return nativeInvoke('peer_delta_status')
        },
        async stop(sessionId: string): Promise<void> {
            requireNative()
            if (options.platform === 'desktop') return nativeInvoke('peer_delta_stop', { sessionId })
            const foreground = await nativeInvoke<PeerDeltaForegroundIdentity | null>('peer_delta_stop', { sessionId })
            if (foreground) {
                if (!bridge) throw new Error('Android peer delta foreground service is unavailable')
                if (!bridge.stopSource(foreground.lane, foreground.operationId, foreground.generation)) {
                    throw new Error('Android peer delta source foreground service could not stop')
                }
            }
        },
        async revoke(sessionId: string, deviceId: string): Promise<void> {
            requireNative()
            return nativeInvoke('peer_delta_revoke', { sessionId, deviceId })
        },
        async pull(pairingUri: string): Promise<PeerDeltaPullResult> {
            requireNative()
            const runtime = options.runtime
            if (!runtime) throw new Error('Peer delta mutation runtime is unavailable')
            const pairing = parsePeerDeltaUri(pairingUri)
            if (pendingRefresh) {
                const pending = pendingRefresh
                if (!samePairing(pending.pairing, pairing)) {
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
                const result = await nativeInvoke<PeerDeltaPullResult>('peer_delta_pull', {
                    ...pairing,
                    expectedRevision: token.revision,
                    ...(foreground ? { foreground } : {}),
                })
                if (result.kind === 'updated' || result.kind === 'noChanges') {
                    pendingRefresh = { pairing, result, fence, foreground }
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
        },
    }
    return facade
}
