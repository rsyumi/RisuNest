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

export type PeerCloneCapability =
    | { kind: 'supported'; platform: 'desktop' }
    | { kind: 'unsupported'; platform: 'web' | 'android' }

export interface PeerCloneSourceStatus {
    sessionId?: string
    manifestId?: string
    pairingUri?: string
    phase: 'idle' | 'prepared' | 'starting' | 'running' | 'stopping' | 'stopped'
    tunnel?: PeerCloneTunnelMetadata
    devices: readonly {
        deviceId: string
        verifiedBytes: number
        currentObject?: string
        lastSeenAt: number
        revoked?: boolean
    }[]
}

export interface PeerCloneTunnelMetadata {
    kind: 'quick' | 'named'
    experimental: boolean
    oneShot: boolean
}

export interface PeerCloneTunnelStatus {
    sessionId?: string
    phase: 'idle' | 'starting' | 'running' | 'stopping' | 'stopped'
    tunnel?: PeerCloneTunnelMetadata
}

export interface PeerCloneTargetStatus {
    phase: 'idle' | 'downloading' | 'cancelling' | 'awaitingActivation' | 'activating' | 'cancelled' | 'completed' | 'failed'
    completedBytes: number
    totalBytes?: number
    error?: string
}

export interface PeerCloneNativeCapabilities {
    desktop: true
    sourceReady: boolean
    atomicActivationReady: boolean
    losslessBackupReady: boolean
    httpTransportReady: boolean
    largeFixturePassed: boolean
    productionEnabled: boolean
}

export interface PeerCloneState {
    source: {
        phase: 'idle' | 'prepared' | 'running' | 'stopped'
        sessionId?: string
        revokedDeviceIds: string[]
    }
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
    | { type: 'source-prepared'; sessionId: string }
    | { type: 'source-started' }
    | { type: 'source-stopped' }
    | { type: 'source-revoked'; deviceId: string }
    | { type: 'target-joined'; pairing: PeerClonePairing }
    | { type: 'target-confirmed' }
    | { type: 'target-progress'; completedBytes: number; totalBytes?: number }
    | { type: 'target-cancelled' }
    | { type: 'target-resumed' }
    | { type: 'target-completed'; backupPaths?: string[] }
    | { type: 'target-failed' }

export const initialPeerCloneState: PeerCloneState = {
    source: { phase: 'idle', revokedDeviceIds: [] },
    target: { phase: 'idle', destructiveConfirmed: false, completedBytes: 0 },
}

export function reducePeerCloneState(state: PeerCloneState, event: PeerCloneEvent): PeerCloneState {
    switch (event.type) {
        case 'source-prepared':
            return { ...state, source: { ...state.source, phase: 'prepared', sessionId: event.sessionId } }
        case 'source-started':
            return { ...state, source: { ...state.source, phase: 'running' } }
        case 'source-stopped':
            return { ...state, source: { ...state.source, phase: 'stopped' } }
        case 'source-revoked':
            return {
                ...state,
                source: {
                    ...state.source,
                    revokedDeviceIds: state.source.revokedDeviceIds.includes(event.deviceId)
                        ? state.source.revokedDeviceIds
                        : [...state.source.revokedDeviceIds, event.deviceId],
                },
            }
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
const claimPattern = /^[0-9a-f]{64}$/
const maximumPairingUriLength = 8192
const maximumEndpointLength = 2048
const maximumClaimLength = 512
const namedTunnelOriginUnavailableMessage = 'Named Tunnel cannot bind loopback port 32145. Stop the app using that port, or use Quick Tunnel / Trusted LAN.'

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

function isLoopbackHost(hostname: string): boolean {
    const ipv4 = parseIpv4(hostname)
    return ipv4?.[0] === 127 || hostname === '[::1]'
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

export function parsePeerLanEndpoint(value: string): string {
    const endpoint = parsePeerEndpoint(value)
    if (endpoint.protocol !== 'http:'
        || !hasExplicitValidPort(value)
        || (!isAllowedLanHost(endpoint.hostname) && !isLoopbackHost(endpoint.hostname))
    ) return invalidPairingUri()
    return endpoint.toString()
}

export interface PeerPairingUriRules {
    /** Lane hostname in `risuailocal://<hostname><pathname>`. */
    hostname: string
    /** Required path, `/v1` for the per-lane pairing URIs and `/v2` for device sync links. */
    pathname: string
    /** Lane-specific rejection, e.g. `throw new Error('Invalid peer delta pairing URI')`. */
    invalid(): never
    /**
     * `hex64Fragment` requires a literal 64-hex claim in the fragment (delta, bidirectional);
     * `encodedHex64` accepts a percent-encoded fragment that decodes to 64 hex (clone).
     */
    claimRule: 'hex64Fragment' | 'encodedHex64'
    /** Whether loopback LAN endpoints are accepted as a fallback (delta, bidirectional). */
    allowLanEndpoint: boolean
    /** Whether the canonical endpoint's trailing slash is stripped (bidirectional wire format). */
    trimTrailingSlash: boolean
}

export function parsePeerPairingUri(value: string, rules: PeerPairingUriRules): PeerClonePairing {
    if (value.length === 0 || value.length > maximumPairingUriLength) return rules.invalid()
    let uri: URL
    try {
        uri = new URL(value)
    } catch {
        return rules.invalid()
    }
    if (uri.protocol !== 'risuailocal:' || uri.hostname !== rules.hostname || uri.pathname !== rules.pathname) {
        return rules.invalid()
    }
    const expectedKeys = ['endpoint', 'session', 'manifest']
    if (
        [...uri.searchParams.keys()].length !== expectedKeys.length
        || expectedKeys.some((key) => uri.searchParams.getAll(key).length !== 1)
        || [...uri.searchParams.keys()].some((key) => !expectedKeys.includes(key))
    ) return rules.invalid()

    const sessionId = uri.searchParams.get('session')!
    const manifestId = uri.searchParams.get('manifest')!
    const fragment = uri.hash.slice(1)
    if (!uuidPattern.test(sessionId) || !sha256Pattern.test(manifestId)) return rules.invalid()
    let claim: string
    if (rules.claimRule === 'encodedHex64') {
        if (!/^claim=[^&=\s]+$/.test(fragment)) return rules.invalid()
        try {
            claim = decodeURIComponent(fragment.slice('claim='.length))
        } catch {
            return rules.invalid()
        }
        if (claim.length > maximumClaimLength || !claimPattern.test(claim)) return rules.invalid()
    } else {
        if (!/^claim=[0-9a-f]{64}$/.test(fragment)) return rules.invalid()
        claim = fragment.slice('claim='.length)
    }
    const endpointValue = uri.searchParams.get('endpoint')!
    let endpoint: string
    try {
        endpoint = parsePeerCloneEndpoint(endpointValue)
    } catch {
        if (!rules.allowLanEndpoint) return rules.invalid()
        try {
            endpoint = parsePeerLanEndpoint(endpointValue)
        } catch {
            return rules.invalid()
        }
    }
    if (rules.trimTrailingSlash && endpoint.endsWith('/')) endpoint = endpoint.slice(0, -1)
    return { endpoint, sessionId, manifestId, claim }
}

export function parsePeerCloneUri(value: string): PeerClonePairing {
    return parsePeerPairingUri(value, {
        hostname: 'peer-clone',
        pathname: '/v1',
        invalid: invalidPairingUri,
        claimRule: 'encodedHex64',
        allowLanEndpoint: false,
        trimTrailingSlash: false,
    })
}

export function pairingUriForQr(pairingUri: string): string {
    return pairingUri
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

function sameTargetIdentity(
    left: { sessionId: string; manifestId: string },
    right: { sessionId: string; manifestId: string },
): boolean {
    return left.sessionId === right.sessionId && left.manifestId === right.manifestId
}

export function createPeerCloneFacade(options: PeerCloneFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    let state = initialPeerCloneState
    let finalization: Promise<PeerCloneTargetStatus> | undefined
    let targetIdentityEpoch = 0
    let warning = ''
    let ownedTarget: { endpoint: string; sessionId: string; manifestId: string } | undefined
    let claimOwned = false
    let pendingRefresh: {
        request: { endpoint: string; sessionId: string; manifestId: string }
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
    const ownTarget = (request: { endpoint: string; sessionId: string; manifestId: string }) => {
        if (ownedTarget && !sameTargetRequest(ownedTarget, request)) {
            throw new Error('Another peer clone target job is already owned')
        }
        ownedTarget ??= request
        return request
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
                if (pendingRefresh && !sameTargetRequest(pendingRefresh.request, request)) {
                    throw new Error('Another peer clone target is awaiting renderer refresh')
                }
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
                    pendingRefresh = { request, revision, fence, rendererRefreshed: false, backupPaths }
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
                ownedTarget = undefined
                claimOwned = false
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
    const requireSourceReady = async () => {
        const current = await capabilities()
        if (
            !current.productionEnabled
            || !current.sourceReady
            || !current.losslessBackupReady
            || !current.httpTransportReady
        ) {
            throw new Error('Peer clone source is not enabled by native production gates')
        }
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
        status(): PeerCloneCapability {
            return options.platform === 'desktop'
                ? { kind: 'supported', platform: 'desktop' }
                : { kind: 'unsupported', platform: options.platform }
        },
        getState: () => state,
        getWarning: () => warning,
        capabilities,
        join(pairingUri: string): PeerCloneState {
            if (finalization || pendingRefresh) {
                throw new Error('Peer clone target finalization is still active')
            }
            const pairing = parsePeerCloneUri(pairingUri)
            if (ownedTarget) {
                const rotatable = state.target.phase === 'failed'
                    || state.target.phase === 'cancelled'
                    || (!claimOwned && (state.target.phase === 'joined' || state.target.phase === 'confirmed'))
                if (rotatable && sameTargetIdentity(ownedTarget, pairing)) {
                    warning = ''
                    targetIdentityEpoch += 1
                    ownedTarget = {
                        endpoint: pairing.endpoint,
                        sessionId: pairing.sessionId,
                        manifestId: pairing.manifestId,
                    }
                    claimOwned = false
                    state = reducePeerCloneState(state, { type: 'target-joined', pairing })
                    return state
                }
                if (!sameTargetRequest(ownedTarget, pairing)) {
                    throw new Error('Another peer clone target job is already owned')
                }
                return state
            }
            warning = ''
            targetIdentityEpoch += 1
            state = reducePeerCloneState(state, { type: 'target-joined', pairing })
            return state
        },
        joinClaimed(target: PeerCloneClaimedTarget): PeerCloneState {
            if (finalization || pendingRefresh) {
                throw new Error('Peer clone target finalization is still active')
            }
            const pairing: PeerClonePairing = { ...target, claim: '' }
            warning = ''
            targetIdentityEpoch += 1
            ownedTarget = { ...target }
            claimOwned = true
            state = reducePeerCloneState(state, { type: 'target-joined', pairing })
            return state
        },
        confirmDestructiveReplace(): PeerCloneState {
            if (!state.target.pairing) throw new Error('Peer clone target has not joined a pairing')
            state = reducePeerCloneState(state, { type: 'target-confirmed' })
            return state
        },
        async prepare(request: Record<string, unknown> = {}): Promise<PeerCloneSourceStatus> {
            supported()
            await requireSourceReady()
            await replacementRuntime().flushPendingData('peer-clone-source-prepare')
            const result = await nativeInvoke<PeerCloneSourceStatus>('peer_clone_prepare', request)
            if (result.sessionId) state = reducePeerCloneState(state, { type: 'source-prepared', sessionId: result.sessionId })
            return result
        },
        async start(sessionId: string): Promise<PeerCloneSourceStatus> {
            supported()
            await requireSourceReady()
            const result = await nativeInvoke<PeerCloneSourceStatus>('peer_clone_start', { sessionId })
            state = reducePeerCloneState(state, { type: 'source-started' })
            return result
        },
        async startQuickTunnel(sessionId: string): Promise<PeerCloneSourceStatus> {
            supported()
            await requireSourceReady()
            const result = await nativeInvoke<PeerCloneSourceStatus>('peer_clone_tunnel_start', {
                sessionId,
                tunnel: { kind: 'quick' },
            })
            state = reducePeerCloneState(state, { type: 'source-started' })
            return result
        },
        async startNamedTunnel(
            sessionId: string,
            token: string,
            expectedPublicBaseUrl: string,
        ): Promise<PeerCloneSourceStatus> {
            supported()
            await requireSourceReady()
            try {
                const result = await nativeInvoke<PeerCloneSourceStatus>('peer_clone_tunnel_start', {
                    sessionId,
                    tunnel: { kind: 'named', token, expectedPublicBaseUrl },
                })
                state = reducePeerCloneState(state, { type: 'source-started' })
                return result
            } catch (cause) {
                const message = cause instanceof Error
                    ? cause.message
                    : typeof cause === 'string' ? cause : ''
                if (message === namedTunnelOriginUnavailableMessage) {
                    throw new Error(namedTunnelOriginUnavailableMessage)
                }
                throw new Error('Named tunnel failed to start')
            }
        },
        async sourceStatus(): Promise<PeerCloneSourceStatus> {
            supported()
            return nativeInvoke('peer_clone_status')
        },
        async tunnelStatus(): Promise<PeerCloneTunnelStatus> {
            supported()
            return nativeInvoke('peer_clone_tunnel_status')
        },
        async stop(sessionId: string): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_stop', { sessionId })
            state = reducePeerCloneState(state, { type: 'source-stopped' })
        },
        async stopTunnel(sessionId: string): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_tunnel_stop', { sessionId })
            state = reducePeerCloneState(state, { type: 'source-stopped' })
        },
        async revoke(sessionId: string, deviceId: string): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_revoke', { sessionId, deviceId })
            state = reducePeerCloneState(state, { type: 'source-revoked', deviceId })
        },
        async download(): Promise<void> {
            supported()
            const pairing = state.target.pairing
            if (!state.target.destructiveConfirmed || !pairing) {
                throw new Error('Peer clone target requires destructive replacement confirmation')
            }
            if (!claimOwned) throw new Error('Peer clone target requires a registered source claim')
            const args = ownTarget({
                endpoint: pairing.endpoint,
                sessionId: pairing.sessionId,
                manifestId: pairing.manifestId,
            })
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
            const args = ownTarget(targetArgs())
            await requireTargetReady()
            await nativeInvoke('peer_clone_resume', args)
            state = reducePeerCloneState(state, { type: 'target-resumed' })
        },
        async cancel(): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_cancel', ownTarget(targetArgs()))
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
        reportTargetProgress(completedBytes: number, totalBytes?: number): PeerCloneState {
            state = reducePeerCloneState(state, { type: 'target-progress', completedBytes, totalBytes })
            return state
        },
        reportTargetFailure(): PeerCloneState {
            state = reducePeerCloneState(state, { type: 'target-failed' })
            return state
        },
    }
}
