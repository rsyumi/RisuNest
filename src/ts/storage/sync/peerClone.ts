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
}

export type PeerCloneCapability =
    | { kind: 'supported'; platform: 'desktop' }
    | { kind: 'unsupported'; platform: 'web' | 'android' }

export interface PeerCloneSourceStatus {
    sessionId?: string
    phase: 'idle' | 'prepared' | 'running' | 'stopped'
    devices: readonly { deviceId: string; verifiedBytes: number; lastSeenAt: number }[]
}

export interface PeerCloneState {
    source: {
        phase: 'idle' | 'prepared' | 'running' | 'stopped'
        sessionId?: string
        revokedDeviceIds: string[]
    }
    target: {
        phase: 'idle' | 'joined' | 'confirmed' | 'downloading' | 'cancelled' | 'completed'
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
    }
}

const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const sha256Pattern = /^[0-9a-f]{64}$/
const hostnamePattern = /^(?=.{1,253}$)(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?)(?:\.(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?))*$/i

function invalidPairingUri(): never {
    throw new Error('Invalid peer clone pairing URI')
}

function isIpLiteral(hostname: string): boolean {
    return /^\d{1,3}(?:\.\d{1,3}){3}$/.test(hostname)
        || /^\[[0-9a-f:.]+\]$/i.test(hostname)
}

function endpointFor(sessionId: string, value: string): string {
    let endpoint: URL
    try {
        endpoint = new URL(value)
    } catch {
        return invalidPairingUri()
    }
    if (
        endpoint.protocol !== 'http:'
        || !endpoint.port
        || endpoint.username
        || endpoint.password
        || endpoint.hash
        || endpoint.search
        || (!isIpLiteral(endpoint.hostname) && !hostnamePattern.test(endpoint.hostname))
    ) return invalidPairingUri()

    const sessionPath = `/v1/sessions/${sessionId}`
    if (endpoint.pathname !== '/' && endpoint.pathname !== sessionPath) return invalidPairingUri()
    return endpoint.toString()
}

export function parsePeerCloneUri(value: string): PeerClonePairing {
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
    return {
        endpoint: endpointFor(sessionId, endpointValue),
        sessionId,
        manifestId,
        claim: decodeURIComponent(fragment.slice('claim='.length)),
    }
}

export function pairingUriForQr(pairingUri: string): string {
    return pairingUri
}

function unsupported(platform: PeerClonePlatform): never {
    throw new Error(`Peer clone is unsupported on ${platform}`)
}

export function createPeerCloneFacade(options: PeerCloneFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    let state = initialPeerCloneState
    const supported = () => options.platform === 'desktop' || unsupported(options.platform)
    const targetArgs = () => {
        if (!state.target.destructiveConfirmed || !state.target.pairing) {
            throw new Error('Peer clone target requires destructive replacement confirmation')
        }
        const { endpoint, sessionId, manifestId } = state.target.pairing
        return { endpoint, sessionId, manifestId }
    }

    return {
        status(): PeerCloneCapability {
            return options.platform === 'desktop'
                ? { kind: 'supported', platform: 'desktop' }
                : { kind: 'unsupported', platform: options.platform }
        },
        getState: () => state,
        join(pairingUri: string): PeerCloneState {
            state = reducePeerCloneState(state, { type: 'target-joined', pairing: parsePeerCloneUri(pairingUri) })
            return state
        },
        confirmDestructiveReplace(): PeerCloneState {
            if (!state.target.pairing) throw new Error('Peer clone target has not joined a pairing')
            state = reducePeerCloneState(state, { type: 'target-confirmed' })
            return state
        },
        async prepare(request: Record<string, unknown> = {}): Promise<PeerCloneSourceStatus> {
            supported()
            const result = await nativeInvoke<PeerCloneSourceStatus>('peer_clone_prepare', request)
            if (result.sessionId) state = reducePeerCloneState(state, { type: 'source-prepared', sessionId: result.sessionId })
            return result
        },
        async start(sessionId: string): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_start', { sessionId })
            state = reducePeerCloneState(state, { type: 'source-started' })
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
            const args = targetArgs()
            const pairing = state.target.pairing!
            await nativeInvoke('peer_clone_claim_client', {
                endpoint: pairing.endpoint,
                sessionId: pairing.sessionId,
                claim: pairing.claim,
            })
            await nativeInvoke('peer_clone_download', args)
            state = reducePeerCloneState(state, { type: 'target-resumed' })
        },
        async resume(): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_resume', targetArgs())
            state = reducePeerCloneState(state, { type: 'target-resumed' })
        },
        async cancel(): Promise<void> {
            supported()
            await nativeInvoke('peer_clone_cancel', targetArgs())
            state = reducePeerCloneState(state, { type: 'target-cancelled' })
        },
        reportTargetProgress(completedBytes: number, totalBytes?: number): PeerCloneState {
            state = reducePeerCloneState(state, { type: 'target-progress', completedBytes, totalBytes })
            return state
        },
    }
}
