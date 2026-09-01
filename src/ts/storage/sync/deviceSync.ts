import { invoke } from '@tauri-apps/api/core'
import { parsePeerPairingUri } from './peerClone'

type DeviceSyncInvoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>

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
    if (message === 'authorizationExpired' || message === 'sourceMissing' || message === 'identityMismatch') {
        return new DeviceSyncError('registration-expired')
    }
    if (message === 'transportUnavailable') return new DeviceSyncError('transport-changed')
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
    if (value.protocol !== 'risuailocal:' || value.hostname !== 'peer-clone' || value.pathname !== '/v2') {
        throw new Error('Invalid device sync link')
    }
    value.pathname = '/v1'
    return parsePeerPairingUri(value.toString(), {
        hostname: 'peer-clone',
        invalid: () => { throw new Error('Invalid device sync link') },
        claimRule: 'hex64Fragment',
        allowLanEndpoint: true,
        trimTrailingSlash: false,
    })
}

function safeStatus(value: unknown): DeviceSyncStatus {
    const source = value && typeof value === 'object' ? value as Record<string, unknown> : {}
    const phase = source.phase
    return {
        phase: phase === 'preparing' || phase === 'prepared' || phase === 'starting' || phase === 'running' || phase === 'stopping' || phase === 'error'
            ? phase
            : 'idle',
        ...(typeof source.endpoint === 'string' ? { endpoint: source.endpoint } : {}),
        ...(typeof source.pairingUri === 'string' ? { pairingUri: source.pairingUri } : {}),
        ...(typeof source.expiresAtMs === 'number' && Number.isFinite(source.expiresAtMs) && source.expiresAtMs >= 0
            ? { expiresAtMs: source.expiresAtMs }
            : {}),
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
    const status = async (): Promise<DeviceSyncStatus> => safeStatus(await safeInvoke(nativeInvoke, 'device_sync_status'))
    const source = async (command: string, settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus> => {
        validate(settings)
        return safeStatus(await safeInvoke(nativeInvoke, command, { request: { ...settings } }))
    }
    return {
        prepare: (settings: DeviceSyncSettingsInput) => source('device_sync_prepare', settings),
        start: async (permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> =>
            safeStatus(await safeInvoke(nativeInvoke, 'device_sync_start', { permissions })),
        status,
        async stop(): Promise<void> {
            await safeInvoke(nativeInvoke, 'device_sync_stop')
        },
        async rotateLink(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> {
            return safeStatus(await safeInvoke(nativeInvoke, 'device_sync_rotate_link', { permissions }))
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
            safeRegisteredCloneSession(await safeInvoke(nativeInvoke, 'peer_clone_claim_client', { ...link })),
        reconnectRegisteredClone: async (deviceId: string): Promise<RegisteredCloneSession> =>
            safeRegisteredCloneSession(await safeInvoke(nativeInvoke, 'peer_clone_claim_registered_client', { deviceId })),
    }
}
