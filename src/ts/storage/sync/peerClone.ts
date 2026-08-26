import { invoke } from '@tauri-apps/api/core'

export type PeerClonePlatform = 'desktop' | 'web' | 'android'

export interface PeerClonePairing {
    endpoint: string
    sessionId: string
    manifestId: string
    claim: string
}

export interface PeerCloneInvoke {
    <T>(command: string, args?: Record<string, unknown>): Promise<T>
}

export interface PeerCloneFacadeOptions {
    platform: PeerClonePlatform
    invoke?: PeerCloneInvoke
    runtime?: PeerCloneReplacementRuntime
}

export interface PeerCloneReplacementRuntime {
    flushPendingData(reason: string): Promise<void>
    capturePersistentMutationToken(reason: string): Promise<{
        revision: number
        mutationGeneration: number
    }>
    acquireDestructiveReplacementFence(token: {
        revision: number
        mutationGeneration: number
    }): Promise<{
        refreshCommittedWorkingSet(revision: number): Promise<void>
        release(): void
    }>
}

export type PeerCloneCapability =
    | { kind: 'supported'; platform: 'desktop' }
    | { kind: 'unsupported'; platform: 'web' | 'android' }

export interface PeerCloneSourceStatus {
    sessionId?: string
    manifestId?: string
    pairingUri?: string
    phase: 'idle' | 'prepared' | 'running' | 'stopping' | 'stopped'
    devices: readonly {
        deviceId: string
        verifiedBytes: number
        currentObject?: string
        lastSeenAt: number
        revoked?: boolean
    }[]
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
    | { type: 'target-completed' }
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
            return { ...state, target: { ...state.target, phase: 'completed' } }
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

function isAllowedLanHost(hostname: string): boolean {
    const ipv4 = parseIpv4(hostname)
    if (!ipv4) return false
    return ipv4[0] === 10
        || (ipv4[0] === 172 && ipv4[1] >= 16 && ipv4[1] <= 31)
        || (ipv4[0] === 192 && ipv4[1] === 168)
        || (ipv4[0] === 169 && ipv4[1] === 254)
}

function endpointFor(sessionId: string, value: string): string {
    if (value.length === 0 || value.length > maximumEndpointLength || !hasExplicitValidPort(value)) {
        return invalidPairingUri()
    }
    let endpoint: URL
    try {
        endpoint = new URL(value)
    } catch {
        return invalidPairingUri()
    }
    const ipv4 = parseIpv4(endpoint.hostname)
    const ipv4Shaped = /^\d+(?:\.\d+){3}$/.test(endpoint.hostname)
    if (
        endpoint.protocol !== 'http:'
        || endpoint.username
        || endpoint.password
        || endpoint.hash
        || endpoint.search
        || (ipv4Shaped && !ipv4)
        || !isAllowedLanHost(endpoint.hostname)
    ) return invalidPairingUri()

    if (endpoint.pathname !== '/') return invalidPairingUri()
    return endpoint.toString()
}

export function parsePeerCloneUri(value: string): PeerClonePairing {
    if (value.length === 0 || value.length > maximumPairingUriLength) return invalidPairingUri()
    let uri: URL
    try {
        uri = new URL(value)
    } catch {
        return invalidPairingUri()
    }
    if (uri.protocol !== 'risuailocal:' || uri.hostname !== 'peer-clone' || uri.pathname !== '/v1') {
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
    const endpointValue = uri.searchParams.get('endpoint')!
    const fragment = uri.hash.slice(1)
    if (!uuidPattern.test(sessionId) || !sha256Pattern.test(manifestId) || !/^claim=[^&=\s]+$/.test(fragment)) {
        return invalidPairingUri()
    }
    let claim: string
    try {
        claim = decodeURIComponent(fragment.slice('claim='.length))
    } catch {
        return invalidPairingUri()
    }
    if (claim.length > maximumClaimLength || !claimPattern.test(claim)) return invalidPairingUri()
    return {
        endpoint: endpointFor(sessionId, endpointValue),
        sessionId,
        manifestId,
        claim,
    }
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

export function createPeerCloneFacade(options: PeerCloneFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    let state = initialPeerCloneState
    let finalization: Promise<PeerCloneTargetStatus> | undefined
    let targetIdentityEpoch = 0
    let warning = ''
    let ownedTarget: { endpoint: string; sessionId: string; manifestId: string } | undefined
    let pendingRefresh: {
        request: { endpoint: string; sessionId: string; manifestId: string }
        revision: number
        fence: Awaited<ReturnType<PeerCloneReplacementRuntime['acquireDestructiveReplacementFence']>>
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
                    const result = await nativeInvoke<{ revision: number; warning?: string }>('peer_clone_finalize', request)
                    revision = result.revision
                    warning = result.warning ?? ''
                    pendingRefresh = { request, revision, fence }
                }
                await fence.refreshCommittedWorkingSet(revision)
                pendingRefresh = undefined
                fence.release()
                fence = undefined
                const completed = { ...awaiting, phase: 'completed' as const }
                state = reducePeerCloneState(state, { type: 'target-completed' })
                ownedTarget = undefined
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
        async sourceStatus(): Promise<PeerCloneSourceStatus> {
            supported()
            return nativeInvoke('peer_clone_status')
        },
        async stop(sessionId: string): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_stop', { sessionId })
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
            const args = ownTarget({
                endpoint: pairing.endpoint,
                sessionId: pairing.sessionId,
                manifestId: pairing.manifestId,
            })
            const pairingClaim = pairing.claim
            await requireTargetReady()
            await nativeInvoke('peer_clone_claim_client', {
                ...args,
                claim: pairingClaim,
            })
            await nativeInvoke('peer_clone_download', args)
            state = reducePeerCloneState(state, { type: 'target-resumed' })
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
                ownedTarget = undefined
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
