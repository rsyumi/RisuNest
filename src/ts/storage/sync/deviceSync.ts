import { invoke } from '@tauri-apps/api/core'

type DeviceSyncInvoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>

export type DeviceSyncMethod = 'lan' | 'quick' | 'fixed-url'
export type DeviceSyncPhase = 'idle' | 'prepared' | 'starting' | 'running' | 'stopping' | 'error'
export type DeviceSyncPermission = 'read' | 'bidirectional'

export interface DeviceSyncSettingsInput {
    method: DeviceSyncMethod
    fixedPort: number
    publicBaseUrl: string
}

export interface DeviceSyncStatus {
    phase: DeviceSyncPhase
    pairingUri?: string
    expiresAtMs?: number
    error?: 'port-unavailable' | 'transport-unavailable' | 'unavailable'
}

export interface RegisteredDevice {
    deviceId: string
    name: string
    permissions: readonly DeviceSyncPermission[]
    lastSeenMs?: number
    totalBytes?: number
}

export interface RegisteredCloneSession {
    endpoint: string
    sessionId: string
    manifestId: string
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
        ...(source.error === 'port-unavailable' || source.error === 'transport-unavailable' || source.error === 'unavailable'
            ? { error: source.error }
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
    const status = async (): Promise<DeviceSyncStatus> => safeStatus(await nativeInvoke('device_sync_status'))
    const source = async (command: string, settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus> => {
        validate(settings)
        return safeStatus(await nativeInvoke(command, { ...settings }))
    }
    return {
        prepare: (settings: DeviceSyncSettingsInput) => source('device_sync_prepare', settings),
        start: (settings: DeviceSyncSettingsInput) => source('device_sync_start', settings),
        status,
        async stop(): Promise<DeviceSyncStatus> {
            return safeStatus(await nativeInvoke('device_sync_stop'))
        },
        async rotateLink(): Promise<DeviceSyncStatus> {
            return safeStatus(await nativeInvoke('device_sync_rotate_link'))
        },
        async outgoingDevices(): Promise<RegisteredDevice[]> {
            return safeDevices(await nativeInvoke('device_sync_registered_devices'))
        },
        async incomingSources(): Promise<RegisteredDevice[]> {
            return safeDevices(await nativeInvoke('device_sync_registered_sources'))
        },
        revokeOutgoing: async (deviceId: string): Promise<void> => {
            await nativeInvoke('device_sync_revoke_device', { deviceId })
        },
        revokeIncoming: async (deviceId: string): Promise<void> => {
            await nativeInvoke('device_sync_revoke_source', { deviceId })
        },
        helloRegistered: async (deviceId: string): Promise<void> => {
            await nativeInvoke('peer_sync_registered_hello', { deviceId })
        },
        claimRegisteredClone: async (deviceId: string): Promise<RegisteredCloneSession> =>
            await nativeInvoke('peer_clone_claim_registered_client', { deviceId }) as RegisteredCloneSession,
        pullRegisteredDelta: async <T>(deviceId: string): Promise<T> =>
            await nativeInvoke('peer_delta_pull_registered', { deviceId }) as T,
        syncRegisteredBidirectional: async <T>(deviceId: string): Promise<T> =>
            await nativeInvoke('peer_bidirectional_sync_registered', { deviceId }) as T,
        resolveRegisteredBidirectional: async <T>(
            deviceId: string,
            operationId: string,
            winner: 'local' | 'remote',
        ): Promise<T> => await nativeInvoke('peer_bidirectional_resolve_registered', { deviceId, operationId, winner }) as T,
    }
}
