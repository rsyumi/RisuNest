import { invoke } from '@tauri-apps/api/core'
import { parsePeerPairingUri } from './peerClone'

type DeviceSyncInvoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>

export type DeviceSyncMethod = 'lan' | 'quick' | 'fixed-url'
export type DeviceSyncPhase = 'idle' | 'prepared' | 'starting' | 'running' | 'stopping' | 'error'
export type DeviceSyncPermission = 'read' | 'bidirectional'
export type DeviceSyncErrorCode = 'port-unavailable' | 'registration-expired' | 'transport-changed' | 'unavailable'

export class DeviceSyncError extends Error {
    constructor(readonly code: DeviceSyncErrorCode) {
        super(code)
    }
}

function safeFailure(error: unknown): DeviceSyncError {
    const message = error instanceof Error ? error.message : String(error)
    if (message.includes('401') || message.includes('expired')) return new DeviceSyncError('registration-expired')
    if (message.includes('port')) return new DeviceSyncError('port-unavailable')
    if (message.includes('transport') || message.includes('endpoint')) return new DeviceSyncError('transport-changed')
    return new DeviceSyncError('unavailable')
}

async function safeInvoke<T>(invoke: DeviceSyncInvoke, command: string, args?: Record<string, unknown>): Promise<T> {
    try {
        return await invoke(command, args) as T
    } catch (error) {
        throw safeFailure(error)
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
    pairingUri?: string
    expiresAtMs?: number
    error?: DeviceSyncErrorCode
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
        phase: phase === 'prepared' || phase === 'starting' || phase === 'running' || phase === 'stopping' || phase === 'error'
            ? phase
            : 'idle',
        ...(typeof source.pairingUri === 'string' ? { pairingUri: source.pairingUri } : {}),
        ...(typeof source.expiresAtMs === 'number' ? { expiresAtMs: source.expiresAtMs } : {}),
        ...(source.error === 'port-unavailable' || source.error === 'transport-changed' || source.error === 'registration-expired' || source.error === 'unavailable'
            ? { error: source.error as DeviceSyncErrorCode }
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
            ...(typeof source.lastSeenMs === 'number' ? { lastSeenMs: source.lastSeenMs } : {}),
            ...(typeof source.totalBytes === 'number' ? { totalBytes: source.totalBytes } : {}),
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
        return safeStatus(await safeInvoke(nativeInvoke, command, { ...settings }))
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
        helloRegistered: async (deviceId: string): Promise<void> => {
            await safeInvoke(nativeInvoke, 'peer_sync_registered_hello', { deviceId })
        },
        claimStagedClone: async (link: StagedDeviceSyncLink): Promise<RegisteredCloneSession> =>
            await safeInvoke<RegisteredCloneSession>(nativeInvoke, 'peer_clone_claim_client', { ...link }),
        pullRegisteredDelta: async <T>(deviceId: string): Promise<T> =>
            await safeInvoke<T>(nativeInvoke, 'peer_delta_pull_registered', { deviceId }),
        syncRegisteredBidirectional: async <T>(deviceId: string): Promise<T> =>
            await safeInvoke<T>(nativeInvoke, 'peer_bidirectional_sync_registered', { deviceId }),
        resolveRegisteredBidirectional: async <T>(
            deviceId: string,
            operationId: string,
            winner: 'local' | 'remote',
        ): Promise<T> => await safeInvoke<T>(nativeInvoke, 'peer_bidirectional_resolve_registered', { deviceId, operationId, winner }),
    }
}
