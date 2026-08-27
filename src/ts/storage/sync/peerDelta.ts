import { invoke } from '@tauri-apps/api/core'

import {
    parsePeerCloneEndpoint,
    type PeerClonePlatform,
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
    desktop: true
    sourceReady: boolean
    atomicActivationReady: boolean
    authenticatedTransportReady: boolean
    productionEnabled: boolean
}

export interface PeerDeltaSourceStatus {
    sessionId?: string
    manifestId?: string
    pairingUri?: string
    phase: 'idle' | 'prepared' | 'running' | 'stopped'
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
    return {
        endpoint: parsePeerCloneEndpoint(uri.searchParams.get('endpoint')!),
        sessionId,
        manifestId,
        claim: fragment.slice('claim='.length),
    }
}

function unsupported(platform: Exclude<PeerClonePlatform, 'desktop'>): never {
    throw new Error(`Peer delta is unsupported on ${platform}`)
}

export function createPeerDeltaFacade(options: {
    platform: PeerClonePlatform
    invoke?: PeerDeltaInvoke
    runtime?: PeerDeltaMutationRuntime
}) {
    const nativeInvoke = options.invoke ?? invoke
    const requireDesktop = (): void => {
        if (options.platform !== 'desktop') unsupported(options.platform)
    }
    return {
        async capabilities(): Promise<PeerDeltaCapabilities> {
            requireDesktop()
            return nativeInvoke('peer_delta_capabilities')
        },
        async prepare(): Promise<PeerDeltaSourceStatus> {
            requireDesktop()
            return nativeInvoke('peer_delta_prepare')
        },
        async start(sessionId: string): Promise<PeerDeltaSourceStatus> {
            requireDesktop()
            return nativeInvoke('peer_delta_start', { sessionId })
        },
        async status(): Promise<PeerDeltaSourceStatus> {
            requireDesktop()
            return nativeInvoke('peer_delta_status')
        },
        async stop(sessionId: string): Promise<void> {
            requireDesktop()
            return nativeInvoke('peer_delta_stop', { sessionId })
        },
        async revoke(sessionId: string, deviceId: string): Promise<void> {
            requireDesktop()
            return nativeInvoke('peer_delta_revoke', { sessionId, deviceId })
        },
        async pull(pairingUri: string): Promise<PeerDeltaPullResult> {
            requireDesktop()
            const runtime = options.runtime
            if (!runtime) throw new Error('Peer delta mutation runtime is unavailable')
            const pairing = parsePeerDeltaUri(pairingUri)
            await runtime.flushPendingData('peer-delta-pull')
            const token = await runtime.capturePersistentMutationToken('peer-delta-pull')
            const fence = await runtime.acquirePersistentMutationFence(token)
            try {
                const result = await nativeInvoke<PeerDeltaPullResult>('peer_delta_pull', {
                    ...pairing,
                    expectedRevision: token.revision,
                })
                if (result.kind === 'updated' || result.kind === 'noChanges') {
                    await fence.refreshCommittedWorkingSet(result.revision)
                }
                return result
            } finally {
                fence.release()
            }
        },
    }
}
