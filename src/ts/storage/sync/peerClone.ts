import { invoke } from '@tauri-apps/api/core'

import type { PeerSyncInvoke, PeerSyncMutationRuntime } from './peerSyncShared'

export type PeerClonePlatform = 'desktop' | 'web' | 'android'

export interface PeerClonePairing {
    endpoint: string
    sessionId: string
    manifestId: string
    claim: string
}

export interface PeerCloneClaimedTarget {
    endpoint: string
    sessionId: string
    manifestId: string
}

export type PeerCloneInvoke = PeerSyncInvoke

export interface PeerCloneFacadeOptions {
    platform: PeerClonePlatform
    invoke?: PeerCloneInvoke
    runtime?: PeerCloneReplacementRuntime
}

export type PeerCloneReplacementRuntime = PeerSyncMutationRuntime

export interface PeerCloneTargetStatus {
    phase: 'idle' | 'downloading' | 'cancelling' | 'awaitingActivation' | 'activating' | 'cancelled' | 'completed' | 'failed'
    completedBytes: number
    totalBytes?: number
    error?: string
}

export interface PeerCloneNativeCapabilities {
    desktop: true
    atomicActivationReady: boolean
    losslessBackupReady: boolean
    httpTransportReady: boolean
    largeFixturePassed: boolean
    productionEnabled: boolean
}

export interface PeerCloneState {
    target: {
        phase: 'idle' | 'joined' | 'confirmed' | 'downloading' | 'cancelled' | 'completed' | 'failed'
        pairing?: PeerClonePairing
        destructiveConfirmed: boolean
        completedBytes: number
        totalBytes?: number
        backupPaths?: string[]
    }
}

export type PeerCloneEvent =
    | { type: 'target-joined'; pairing: PeerClonePairing }
    | { type: 'target-confirmed' }
    | { type: 'target-progress'; completedBytes: number; totalBytes?: number }
    | { type: 'target-cancelled' }
    | { type: 'target-resumed' }
    | { type: 'target-completed'; backupPaths?: string[] }
    | { type: 'target-failed' }

export const initialPeerCloneState: PeerCloneState = {
    target: { phase: 'idle', destructiveConfirmed: false, completedBytes: 0 },
}

export function reducePeerCloneState(state: PeerCloneState, event: PeerCloneEvent): PeerCloneState {
    switch (event.type) {
        case 'target-joined':
            return {
                ...state,
                target: {
                    phase: 'joined',
                    pairing: event.pairing,
                    destructiveConfirmed: false,
                    completedBytes: 0,
                },
            }
        case 'target-confirmed':
            return { ...state, target: { ...state.target, phase: 'confirmed', destructiveConfirmed: true } }
        case 'target-progress':
            return {
                ...state,
                target: {
                    ...state.target,
                    phase: 'downloading',
                    completedBytes: event.completedBytes,
                    totalBytes: event.totalBytes,
                },
            }
        case 'target-cancelled':
            return { ...state, target: { ...state.target, phase: 'cancelled' } }
        case 'target-resumed':
            return { ...state, target: { ...state.target, phase: 'downloading' } }
        case 'target-completed':
            return {
                ...state,
                target: {
                    ...state.target,
                    phase: 'completed',
                    ...(event.backupPaths === undefined ? {} : { backupPaths: event.backupPaths }),
                },
            }
        case 'target-failed':
            return { ...state, target: { ...state.target, phase: 'failed' } }
    }
}

const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const sha256Pattern = /^[0-9a-f]{64}$/
const maximumPairingUriLength = 8192
const maximumEndpointLength = 2048

function invalidPairingUri(): never {
    throw new Error('Invalid peer clone pairing URI')
}

function parseIpv4(hostname: string): number[] | null {
    if (!/^\d{1,3}(?:\.\d{1,3}){3}$/.test(hostname)) return null
    const octets = hostname.split('.').map(Number)
    return octets.every((octet) => octet <= 255) ? octets : null
}

function hasExplicitValidPort(value: string): boolean {
    const match = /^http:\/\/(?:\[[^\]]+\]|[^/:?#]+):(\d+)(?:[/?#]|$)/i.exec(value)
    if (!match) return false
    const port = Number(match[1])
    return Number.isInteger(port) && port >= 1 && port <= 65535
}

function hasExplicitAuthorityPort(value: string): boolean {
    const authority = value.slice(value.indexOf('://') + 3).split(/[/?#]/, 1)[0]
    return authority.slice(authority.lastIndexOf('@') + 1).includes(':')
}

function hasBareAuthorityPath(value: string): boolean {
    const remainder = value.slice(value.indexOf('://') + 3)
    const pathStart = remainder.indexOf('/')
    if (pathStart < 0) return true
    const pathEndOffset = remainder.slice(pathStart).search(/[?#]/)
    const pathEnd = pathEndOffset < 0 ? remainder.length : pathStart + pathEndOffset
    return remainder.slice(pathStart, pathEnd) === '/'
}

function isAllowedLanHost(hostname: string): boolean {
    const ipv4 = parseIpv4(hostname)
    if (ipv4) {
        return ipv4[0] === 10
            || (ipv4[0] === 172 && ipv4[1] >= 16 && ipv4[1] <= 31)
            || (ipv4[0] === 192 && ipv4[1] === 168)
            || (ipv4[0] === 169 && ipv4[1] === 254)
    }
    if (!hostname.startsWith('[') || !hostname.endsWith(']')) return false
    const firstHextet = hostname.slice(1, -1).split(':', 1)[0]
    if (!/^[0-9a-f]{1,4}$/i.test(firstHextet)) return false
    const first = Number.parseInt(firstHextet, 16)
    return (first & 0xfe00) === 0xfc00 || (first & 0xffc0) === 0xfe80
}

function isAllowedPublicHttpsHost(hostname: string): boolean {
    if (parseIpv4(hostname) || /^\d+(?:\.\d+){3}$/.test(hostname)) return false
    return /^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/i.test(hostname)
        && hostname.toLowerCase() !== 'localhost'
}

function parsePeerEndpoint(value: string): URL {
    if (value.length === 0 || value.length > maximumEndpointLength) {
        return invalidPairingUri()
    }
    let endpoint: URL
    try {
        endpoint = new URL(value)
    } catch {
        return invalidPairingUri()
    }
    if (endpoint.username
        || endpoint.password
        || endpoint.hash
        || endpoint.search
        || endpoint.pathname !== '/'
    ) return invalidPairingUri()

    return endpoint
}

export function parsePeerCloneEndpoint(value: string): string {
    const endpoint = parsePeerEndpoint(value)

    const lan = endpoint.protocol === 'http:'
        && hasExplicitValidPort(value)
        && isAllowedLanHost(endpoint.hostname)
    const tunnel = endpoint.protocol === 'https:'
        && !hasExplicitAuthorityPort(value)
        && hasBareAuthorityPath(value)
        && isAllowedPublicHttpsHost(endpoint.hostname)
    if (!lan && !tunnel) return invalidPairingUri()
    return endpoint.toString()
}

/**
 * Parses the single device sync link shape: `risunestlocal://peer-clone/v2` with
 * exactly `endpoint`, `session` and `manifest` query keys and a literal 64-hex
 * claim fragment. `invalid` carries the caller's own rejection wording.
 */
export function parsePeerPairingUri(value: string, invalid: () => never): PeerClonePairing {
    if (value.length === 0 || value.length > maximumPairingUriLength) return invalid()
    let uri: URL
    try {
        uri = new URL(value)
    } catch {
        return invalid()
    }
    if (uri.protocol !== 'risunestlocal:' || uri.hostname !== 'peer-clone' || uri.pathname !== '/v2') {
        return invalid()
    }
    const expectedKeys = ['endpoint', 'session', 'manifest']
    if (
        [...uri.searchParams.keys()].length !== expectedKeys.length
        || expectedKeys.some((key) => uri.searchParams.getAll(key).length !== 1)
        || [...uri.searchParams.keys()].some((key) => !expectedKeys.includes(key))
    ) return invalid()

    const sessionId = uri.searchParams.get('session')!
    const manifestId = uri.searchParams.get('manifest')!
    const fragment = uri.hash.slice(1)
    if (!uuidPattern.test(sessionId) || !sha256Pattern.test(manifestId)) return invalid()
    if (!/^claim=[0-9a-f]{64}$/.test(fragment)) return invalid()
    const claim = fragment.slice('claim='.length)
    let endpoint: string
    try {
        endpoint = parsePeerCloneEndpoint(uri.searchParams.get('endpoint')!)
    } catch {
        return invalid()
    }
    return { endpoint, sessionId, manifestId, claim }
}

function unsupported(platform: PeerClonePlatform): never {
    throw new Error(`Peer clone is unsupported on ${platform}`)
}

function sameTargetRequest(
    left: { endpoint: string; sessionId: string; manifestId: string },
    right: { endpoint: string; sessionId: string; manifestId: string },
): boolean {
    return left.endpoint === right.endpoint
        && left.sessionId === right.sessionId
        && left.manifestId === right.manifestId
}

export function createPeerCloneFacade(options: PeerCloneFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    let state = initialPeerCloneState
    let finalization: Promise<PeerCloneTargetStatus> | undefined
    let targetIdentityEpoch = 0
    let warning = ''
    let pendingRefresh: {
        revision: number
        fence: Awaited<ReturnType<PeerCloneReplacementRuntime['acquireDestructiveReplacementFence']>>
        rendererRefreshed: boolean
        backupPaths?: string[]
    } | undefined
    const supported = () => options.platform === 'desktop' || unsupported(options.platform)
    const targetArgs = () => {
        if (!state.target.destructiveConfirmed || !state.target.pairing) {
            throw new Error('Peer clone target requires destructive replacement confirmation')
        }
        const { endpoint, sessionId, manifestId } = state.target.pairing
        return { endpoint, sessionId, manifestId }
    }
    const capabilities = async (): Promise<PeerCloneNativeCapabilities> => {
        supported()
        return nativeInvoke('peer_clone_capabilities')
    }
    const replacementRuntime = (): PeerCloneReplacementRuntime => {
        if (!options.runtime) throw new Error('Peer clone requires the persistent replacement runtime')
        return options.runtime
    }
    const finalizeTarget = (
        awaiting: PeerCloneTargetStatus,
        request: { endpoint: string; sessionId: string; manifestId: string },
    ): Promise<PeerCloneTargetStatus> => {
        finalization ??= (async () => {
            const runtime = replacementRuntime()
            let fence = pendingRefresh?.fence
            let revision = pendingRefresh?.revision
            let rendererRefreshed = pendingRefresh?.rendererRefreshed ?? false
            let backupPaths = pendingRefresh?.backupPaths
            let nativeStarted = revision !== undefined
            try {
                if (!fence) {
                    const token = await runtime.capturePersistentMutationToken('peer-clone-target-finalize')
                    fence = await runtime.acquireDestructiveReplacementFence(token)
                }
                if (revision === undefined) {
                    nativeStarted = true
                    const result = await nativeInvoke<{
                        revision: number
                        warning?: string
                        backupPath?: string
                    }>('peer_clone_finalize', request)
                    revision = result.revision
                    warning = result.warning ?? ''
                    backupPaths = result.backupPath === undefined ? undefined : [result.backupPath]
                    pendingRefresh = { revision, fence, rendererRefreshed: false, backupPaths }
                    state = {
                        ...state,
                        target: {
                            ...state.target,
                            ...(backupPaths === undefined ? {} : { backupPaths }),
                        },
                    }
                }
                if (!rendererRefreshed) {
                    await fence.refreshCommittedWorkingSet(revision)
                    rendererRefreshed = true
                    if (pendingRefresh) pendingRefresh.rendererRefreshed = true
                }
                await nativeInvoke('peer_clone_release_target', request)
                pendingRefresh = undefined
                fence.release()
                fence = undefined
                const completed = { ...awaiting, phase: 'completed' as const }
                state = reducePeerCloneState(state, { type: 'target-completed', backupPaths })
                return completed
            } catch (error) {
                if (nativeStarted && !pendingRefresh) {
                    state = reducePeerCloneState(state, { type: 'target-failed' })
                }
                throw error
            } finally {
                if (!pendingRefresh) fence?.release()
                finalization = undefined
            }
        })()
        return finalization
    }
    const requireTargetReady = async () => {
        const current = await capabilities()
        if (
            !current.productionEnabled
            || !current.atomicActivationReady
            || !current.losslessBackupReady
            || !current.httpTransportReady
        ) {
            throw new Error('Peer clone target is not enabled by native production gates')
        }
    }

    return {
        getState: () => state,
        getWarning: () => warning,
        capabilities,
        joinClaimed(target: PeerCloneClaimedTarget): PeerCloneState {
            if (finalization || pendingRefresh) {
                throw new Error('Peer clone target finalization is still active')
            }
            const pairing: PeerClonePairing = { ...target, claim: '' }
            warning = ''
            targetIdentityEpoch += 1
            state = reducePeerCloneState(state, { type: 'target-joined', pairing })
            return state
        },
        confirmDestructiveReplace(): PeerCloneState {
            if (!state.target.pairing) throw new Error('Peer clone target has not joined a pairing')
            state = reducePeerCloneState(state, { type: 'target-confirmed' })
            return state
        },
        async download(): Promise<void> {
            supported()
            const pairing = state.target.pairing
            if (!state.target.destructiveConfirmed || !pairing) {
                throw new Error('Peer clone target requires destructive replacement confirmation')
            }
            const args = {
                endpoint: pairing.endpoint,
                sessionId: pairing.sessionId,
                manifestId: pairing.manifestId,
            }
            try {
                await requireTargetReady()
                await nativeInvoke('peer_clone_download', args)
                state = reducePeerCloneState(state, { type: 'target-resumed' })
            } catch (error) {
                state = reducePeerCloneState(state, { type: 'target-failed' })
                throw error
            }
        },
        async resume(): Promise<void> {
            supported()
            const args = targetArgs()
            await requireTargetReady()
            await nativeInvoke('peer_clone_resume', args)
            state = reducePeerCloneState(state, { type: 'target-resumed' })
        },
        async cancel(): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_cancel', targetArgs())
            targetIdentityEpoch += 1
            state = reducePeerCloneState(state, { type: 'target-cancelled' })
        },
        async targetStatus(): Promise<PeerCloneTargetStatus> {
            supported()
            const pairing = state.target.pairing
            if (!pairing) throw new Error('Peer clone target has not joined a pairing')
            const request = {
                endpoint: pairing.endpoint,
                sessionId: pairing.sessionId,
                manifestId: pairing.manifestId,
            }
            const identityEpoch = targetIdentityEpoch
            const result = await nativeInvoke<PeerCloneTargetStatus>('peer_clone_target_status', {
                ...request,
            })
            if (identityEpoch !== targetIdentityEpoch
                || !state.target.pairing
                || !sameTargetRequest(state.target.pairing, request)) return result
            if (result.phase === 'awaitingActivation') {
                state = reducePeerCloneState(state, {
                    type: 'target-progress',
                    completedBytes: result.completedBytes,
                    totalBytes: result.totalBytes,
                })
                return finalizeTarget(result, request)
            } else if (result.phase === 'completed' && pendingRefresh) {
                return finalizeTarget(result, request)
            } else if (result.phase === 'downloading' || result.phase === 'activating') {
                state = reducePeerCloneState(state, {
                    type: 'target-progress',
                    completedBytes: result.completedBytes,
                    totalBytes: result.totalBytes,
                })
            } else if (result.phase === 'cancelled') {
                state = reducePeerCloneState(state, { type: 'target-cancelled' })
            } else if (result.phase === 'completed') {
                state = reducePeerCloneState(state, { type: 'target-completed' })
            } else if (result.phase === 'failed') {
                state = reducePeerCloneState(state, { type: 'target-failed' })
            }
            return result
        },
    }
}
