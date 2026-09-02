import { invoke } from '@tauri-apps/api/core'
import { parsePeerCloneEndpoint, parsePeerPairingUri } from './peerClone'

export type DeviceSyncInvoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>

export type DeviceSyncMethod = 'lan' | 'quick' | 'fixed-url'
export type DeviceSyncPhase = 'idle' | 'preparing' | 'prepared' | 'starting' | 'running' | 'stopping' | 'error'
export type DeviceSyncPermission = 'read' | 'bidirectional'
export type DeviceSyncErrorCode =
    | 'invalid-configuration'
    | 'port-unavailable'
    | 'preparation-failed'
    | 'transport-unavailable'
    | 'cleanup-failed'
    | 'state-unavailable'
    | 'registration-expired'
    | 'transport-changed'
    | 'operation-failed'
    | 'unavailable'

export class DeviceSyncError extends Error {
    constructor(readonly code: DeviceSyncErrorCode) {
        super(code)
    }
}

export function classifyDeviceSyncFailure(error: unknown): DeviceSyncError {
    if (error instanceof DeviceSyncError) return error
    const message = error instanceof Error ? error.message : String(error)
    if (message === 'authorizationExpired' || message === 'sourceMissing') {
        return new DeviceSyncError('registration-expired')
    }
    if (message === 'identityMismatch') return new DeviceSyncError('transport-changed')
    if (message === 'transportUnavailable') return new DeviceSyncError('transport-unavailable')
    if (message === 'permissionDenied' || message === 'laneUnavailable') {
        return new DeviceSyncError('operation-failed')
    }
    if (
        message === 'invalid-configuration'
        || message === 'port-unavailable'
        || message === 'preparation-failed'
        || message === 'transport-unavailable'
        || message === 'cleanup-failed'
        || message === 'state-unavailable'
    ) return new DeviceSyncError(message)
    return new DeviceSyncError('operation-failed')
}

async function safeInvoke<T>(invoke: DeviceSyncInvoke, command: string, args?: Record<string, unknown>): Promise<T> {
    try {
        return await invoke(command, args) as T
    } catch (error) {
        throw classifyDeviceSyncFailure(error)
    }
}

export interface DeviceSyncSettingsInput {
    method: DeviceSyncMethod
    fixedPort: number
    publicBaseUrl: string
}

export interface DeviceSyncLinkPermissions {
    read: boolean
    bidirectional: boolean
}

export interface DeviceSyncStatus {
    phase: DeviceSyncPhase
    endpoint?: string
    pairingUri?: string
    expiresAtMs?: number
    latestError?: DeviceSyncErrorCode
}

export interface RegisteredDevice {
    deviceId: string
    name: string
    permissions: readonly DeviceSyncPermission[]
    lastSeenMs?: number
    totalBytes?: number
}

export interface RegisteredCloneSession {
    sourceDeviceId: string
    endpoint: string
    sessionId: string
    manifestId: string
}

function safeRegisteredCloneSession(value: unknown): RegisteredCloneSession {
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
        throw new DeviceSyncError('unavailable')
    }
    const source = value as Record<string, unknown>
    const keys = Object.keys(source).sort()
    const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i
    const sha256 = /^[0-9a-f]{64}$/i
    let endpoint: URL | undefined
    try {
        endpoint = typeof source.endpoint === 'string' ? new URL(source.endpoint) : undefined
    } catch {
        endpoint = undefined
    }
    if (
        keys.join(',') !== 'endpoint,manifestId,sessionId,sourceDeviceId'
        || typeof source.sourceDeviceId !== 'string'
        || !uuid.test(source.sourceDeviceId)
        || typeof source.endpoint !== 'string'
        || !endpoint
        || (endpoint.protocol !== 'http:' && endpoint.protocol !== 'https:')
        || endpoint.username !== ''
        || endpoint.password !== ''
        || typeof source.sessionId !== 'string'
        || !uuid.test(source.sessionId)
        || typeof source.manifestId !== 'string'
        || !sha256.test(source.manifestId)
    ) throw new DeviceSyncError('unavailable')
    return {
        sourceDeviceId: source.sourceDeviceId,
        endpoint: source.endpoint,
        sessionId: source.sessionId,
        manifestId: source.manifestId,
    }
}

export interface StagedDeviceSyncLink {
    endpoint: string
    sessionId: string
    manifestId: string
    claim: string
}

export function parseDeviceSyncUri(uri: string): StagedDeviceSyncLink {
    let value: URL
    try {
        value = new URL(uri)
    } catch {
        throw new Error('Invalid device sync link')
    }
    if (
        value.protocol !== 'risuailocal:'
        || value.hostname !== 'peer-clone'
        || value.pathname !== '/v2'
        || value.username !== ''
        || value.password !== ''
        || value.port !== ''
    ) {
        throw new Error('Invalid device sync link')
    }
    value.pathname = '/v1'
    return parsePeerPairingUri(value.toString(), {
        hostname: 'peer-clone',
        invalid: () => { throw new Error('Invalid device sync link') },
        claimRule: 'hex64Fragment',
        allowLanEndpoint: false,
        trimTrailingSlash: false,
    })
}

export function safeDeviceSyncStatus(value: unknown): DeviceSyncStatus {
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
        throw new DeviceSyncError('state-unavailable')
    }
    const source = value as Record<string, unknown>
    const phase = source.phase
    if (
        phase !== 'idle'
        && phase !== 'preparing'
        && phase !== 'prepared'
        && phase !== 'starting'
        && phase !== 'running'
        && phase !== 'stopping'
        && phase !== 'error'
    ) throw new DeviceSyncError('state-unavailable')
    let endpoint: string | undefined
    let rawEndpoint: string | undefined
    if (source.endpoint !== undefined && source.endpoint !== null) {
        if (typeof source.endpoint !== 'string') throw new DeviceSyncError('state-unavailable')
        try {
            endpoint = parsePeerCloneEndpoint(source.endpoint)
            rawEndpoint = source.endpoint
        } catch {
            throw new DeviceSyncError('state-unavailable')
        }
    }
    let pairingUri: string | undefined
    if (source.pairingUri !== undefined && source.pairingUri !== null) {
        if (typeof source.pairingUri !== 'string' || !endpoint || phase !== 'running') {
            throw new DeviceSyncError('state-unavailable')
        }
        try {
            const pairing = parseDeviceSyncUri(source.pairingUri)
            const parsedUri = new URL(source.pairingUri)
            const pairingEndpoint = parsedUri.searchParams.get('endpoint')
            const sessionId = parsedUri.searchParams.get('session')
            const manifestId = parsedUri.searchParams.get('manifest')
            if (
                pairing.endpoint !== endpoint
                || pairingEndpoint !== rawEndpoint
                || !sessionId
                || !manifestId
            ) throw new DeviceSyncError('state-unavailable')
            const query = new URLSearchParams()
            query.append('endpoint', pairingEndpoint)
            query.append('session', sessionId)
            query.append('manifest', manifestId)
            const canonical = `risuailocal://peer-clone/v2?${query.toString()}#claim=${pairing.claim}`
            if (source.pairingUri !== canonical) throw new DeviceSyncError('state-unavailable')
            pairingUri = canonical
        } catch {
            throw new DeviceSyncError('state-unavailable')
        }
    }
    let expiresAtMs: number | undefined
    if (source.expiresAtMs !== undefined && source.expiresAtMs !== null) {
        if (
            typeof source.expiresAtMs !== 'number'
            || !Number.isFinite(source.expiresAtMs)
            || source.expiresAtMs < 0
        ) throw new DeviceSyncError('state-unavailable')
        expiresAtMs = source.expiresAtMs
    }
    return {
        phase,
        ...(endpoint ? { endpoint } : {}),
        ...(pairingUri ? { pairingUri } : {}),
        ...(expiresAtMs === undefined ? {} : { expiresAtMs }),
        ...(source.latestError === 'invalid-configuration'
            || source.latestError === 'port-unavailable'
            || source.latestError === 'preparation-failed'
            || source.latestError === 'transport-unavailable'
            || source.latestError === 'cleanup-failed'
            || source.latestError === 'state-unavailable'
            ? { latestError: source.latestError as DeviceSyncErrorCode }
            : {}),
    }
}

function safeDevices(value: unknown): RegisteredDevice[] {
    if (!Array.isArray(value)) return []
    return value.flatMap((entry): RegisteredDevice[] => {
        if (!entry || typeof entry !== 'object') return []
        const source = entry as Record<string, unknown>
        if (typeof source.deviceId !== 'string' || typeof source.name !== 'string') return []
        const permissions = Array.isArray(source.permissions)
            ? source.permissions.filter((permission): permission is DeviceSyncPermission => permission === 'read' || permission === 'bidirectional')
            : []
        return [{
            deviceId: source.deviceId,
            name: source.name,
            permissions,
            ...(typeof source.lastSeenMs === 'number' && Number.isFinite(source.lastSeenMs) && source.lastSeenMs >= 0
                ? { lastSeenMs: source.lastSeenMs }
                : {}),
            ...(typeof source.totalBytes === 'number' && Number.isFinite(source.totalBytes) && source.totalBytes >= 0
                ? { totalBytes: source.totalBytes }
                : {}),
        }]
    })
}

export function createDeviceSyncFacade(options: { invoke?: DeviceSyncInvoke } = {}) {
    const nativeInvoke = options.invoke ?? invoke
    const validate = (settings: DeviceSyncSettingsInput): void => {
        if (!Number.isInteger(settings.fixedPort) || settings.fixedPort < 1 || settings.fixedPort > 65535) {
            throw new Error('Choose a valid port')
        }
    }
    const status = async (): Promise<DeviceSyncStatus> => safeDeviceSyncStatus(await safeInvoke(nativeInvoke, 'device_sync_status'))
    const source = async (command: string, settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus> => {
        validate(settings)
        return safeDeviceSyncStatus(await safeInvoke(nativeInvoke, command, { request: { ...settings } }))
    }
    return {
        prepare: (settings: DeviceSyncSettingsInput) => source('device_sync_prepare', settings),
        start: async (permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> =>
            safeDeviceSyncStatus(await safeInvoke(nativeInvoke, 'device_sync_start', { permissions })),
        status,
        async stop(): Promise<void> {
            await safeInvoke(nativeInvoke, 'device_sync_stop')
        },
        async rotateLink(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> {
            return safeDeviceSyncStatus(await safeInvoke(nativeInvoke, 'device_sync_rotate_link', { permissions }))
        },
        async outgoingDevices(): Promise<RegisteredDevice[]> {
            return safeDevices(await safeInvoke(nativeInvoke, 'peer_sync_outgoing_devices'))
        },
        async incomingSources(): Promise<RegisteredDevice[]> {
            return safeDevices(await safeInvoke(nativeInvoke, 'peer_sync_incoming_sources'))
        },
        revokeOutgoing: async (deviceId: string): Promise<void> => {
            await safeInvoke(nativeInvoke, 'peer_sync_revoke_outgoing_device', { deviceId })
        },
        revokeIncoming: async (deviceId: string): Promise<void> => {
            await safeInvoke(nativeInvoke, 'peer_sync_remove_incoming_source', { deviceId })
        },
        claimStagedClone: async (link: StagedDeviceSyncLink): Promise<RegisteredCloneSession> =>
            safeRegisteredCloneSession(await safeInvoke(nativeInvoke, 'peer_clone_claim_v2_client', { ...link })),
        reconnectRegisteredClone: async (deviceId: string): Promise<RegisteredCloneSession> =>
            safeRegisteredCloneSession(await safeInvoke(nativeInvoke, 'peer_clone_claim_registered_client', { deviceId })),
    }
}
