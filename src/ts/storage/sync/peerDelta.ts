import { invoke } from '@tauri-apps/api/core'

import {
    parsePeerCloneEndpoint,
    parsePeerLanEndpoint,
    type PeerClonePlatform,
    type PeerCloneTunnelMetadata,
    type PeerCloneTunnelStatus,
} from './peerClone'

export interface PeerDeltaPairing {
    endpoint: string
    sessionId: string
    manifestId: string
    claim: string
}

export interface PeerDeltaInvoke {
    <T>(command: string, args?: Record<string, unknown>): Promise<T>
}

export interface PeerDeltaForegroundBridge {
    startSource(lane: string, operationId: string, generation: number): boolean
    stopSource(lane: string, operationId: string, generation: number): boolean
}

interface PeerDeltaForegroundIdentity {
    lane: 'p4-source' | 'p4-target'
    operationId: string
    generation: number
}

interface PeerDeltaTargetForegroundStatus {
    foreground: PeerDeltaForegroundIdentity
    result?: PeerDeltaPullResult
}

export interface PeerDeltaMutationRuntime {
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

const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const sha256Pattern = /^[0-9a-f]{64}$/
const maximumPairingUriLength = 8192

function invalidPairingUri(): never {
    throw new Error('Invalid peer delta pairing URI')
}

export function parsePeerDeltaUri(value: string): PeerDeltaPairing {
    if (value.length === 0 || value.length > maximumPairingUriLength) return invalidPairingUri()
    let uri: URL
    try {
        uri = new URL(value)
    } catch {
        return invalidPairingUri()
    }
    if (uri.protocol !== 'risuailocal:' || uri.hostname !== 'peer-delta' || uri.pathname !== '/v1') {
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
    if (!uuidPattern.test(sessionId)
        || !sha256Pattern.test(manifestId)
        || !/^claim=[0-9a-f]{64}$/.test(fragment)) return invalidPairingUri()
    let endpoint: string
    try {
        endpoint = parsePeerCloneEndpoint(uri.searchParams.get('endpoint')!)
    } catch {
        try {
            endpoint = parsePeerLanEndpoint(uri.searchParams.get('endpoint')!)
        } catch {
            return invalidPairingUri()
        }
    }
    return {
        endpoint,
        sessionId,
        manifestId,
        claim: fragment.slice('claim='.length),
    }
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
        fence: Awaited<ReturnType<PeerDeltaMutationRuntime['acquirePersistentMutationFence']>>
        foreground?: PeerDeltaForegroundIdentity
    } | undefined
    const requireDesktop = (): void => {
        if (options.platform !== 'desktop') unsupported(options.platform)
    }
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
    const recoverTargetForeground = async (): Promise<void> => {
        if (options.platform !== 'android') return
        const pending = await nativeInvoke<PeerDeltaTargetForegroundStatus | null>(
            'peer_delta_target_foreground_status',
        )
        if (!pending) return
        const runtime = options.runtime
        if (pending.result?.kind === 'updated' || pending.result?.kind === 'noChanges') {
            if (!runtime) throw new Error('Peer delta mutation runtime is unavailable')
            await runtime.flushPendingData('peer-delta-target-recovery')
            const token = await runtime.capturePersistentMutationToken('peer-delta-target-recovery')
            const fence = await runtime.acquirePersistentMutationFence(token)
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
            return nativeInvoke('peer_delta_prepare')
        },
        async start(sessionId: string): Promise<PeerDeltaSourceStatus> {
            requireNative()
            if (options.platform === 'desktop') return nativeInvoke('peer_delta_start', { sessionId })
            if (!bridge) throw new Error('Android peer delta foreground service is unavailable')
            const foreground = await nativeInvoke<PeerDeltaForegroundIdentity>('peer_delta_source_reserve')
            if (!bridge.startSource(foreground.lane, foreground.operationId, foreground.generation)) {
                throw new Error('Android peer delta foreground service could not start')
            }
            try {
                return await nativeInvoke('peer_delta_start', { sessionId, foreground })
            } catch (error) {
                bridge.stopSource(foreground.lane, foreground.operationId, foreground.generation)
                throw error
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
            if (foreground && bridge) {
                bridge.stopSource(foreground.lane, foreground.operationId, foreground.generation)
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
            const fence = await runtime.acquirePersistentMutationFence(token)
            let foreground: PeerDeltaForegroundIdentity | undefined
            let fenceReleased = false
            try {
                if (options.platform === 'android') {
                    if (!bridge) throw new Error('Android peer delta foreground service is unavailable')
                    foreground = await nativeInvoke<PeerDeltaForegroundIdentity>('peer_delta_target_reserve')
                    if (!bridge.startSource(foreground.lane, foreground.operationId, foreground.generation)) {
                        await nativeInvoke<boolean>('peer_delta_target_foreground_release', { foreground })
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
            } finally {
                if (!fenceReleased && pendingRefresh?.fence !== fence) fence.release()
            }
        },
    }
    if (options.platform === 'android') {
        const androidFacade = facade as Partial<typeof facade>
        delete androidFacade.startQuickTunnel
        delete androidFacade.startNamedTunnel
        delete androidFacade.tunnelStatus
        delete androidFacade.stopTunnel
    }
    return facade
}
