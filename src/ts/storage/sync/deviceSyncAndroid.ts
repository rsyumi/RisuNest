import { invoke } from '@tauri-apps/api/core'

import {
    classifyDeviceSyncFailure,
    createDeviceSyncFacade,
    DeviceSyncError,
    safeDeviceSyncStatus,
    type DeviceSyncInvoke,
    type DeviceSyncLinkPermissions,
    type DeviceSyncMutationRuntime,
    type DeviceSyncSettingsInput,
    type DeviceSyncStatus,
} from './deviceSync'
import { createPeerAndroidSourceForeground } from './peerAndroidSourceForeground'
import type {
    PeerSyncForegroundBridge,
    PeerSyncInvoke,
} from './peerSyncShared'

export interface AndroidDeviceSyncForegroundIdentity {
    lane: 'device-sync-source'
    operationId: string
    generation: number
}

export interface AndroidDeviceSyncFacadeOptions {
    invoke?: DeviceSyncInvoke
    bridge?: PeerSyncForegroundBridge
    runtime: DeviceSyncMutationRuntime
}

function nativeBridge(): PeerSyncForegroundBridge {
    const bridge = typeof window === 'undefined' ? undefined : window.RisuPeerCloneBridge
    if (!bridge) throw new DeviceSyncError('unavailable')
    return bridge
}

function safeForegroundIdentity(value: unknown): AndroidDeviceSyncForegroundIdentity | null {
    if (value === null || value === undefined) return null
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
        throw new DeviceSyncError('state-unavailable')
    }
    const source = value as Record<string, unknown>
    const canonicalUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
    if (
        Object.keys(source).sort().join(',') !== 'generation,lane,operationId'
        || source.lane !== 'device-sync-source'
        || typeof source.operationId !== 'string'
        || !canonicalUuid.test(source.operationId)
        || typeof source.generation !== 'number'
        || !Number.isSafeInteger(source.generation)
        || source.generation <= 0
    ) throw new DeviceSyncError('state-unavailable')
    return {
        lane: 'device-sync-source',
        operationId: source.operationId,
        generation: source.generation,
    }
}

export function createAndroidDeviceSyncFacade(options: AndroidDeviceSyncFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    const bridge = options.bridge ?? nativeBridge()
    const desktopShape = createDeviceSyncFacade({ invoke: nativeInvoke, runtime: options.runtime })
    // `peer_clone_claim_registered_client` answers with the Android registered clone status
    // instead of the desktop registered clone session, so the Android facade never advertises
    // the desktop reconnect entry point. The clone target's `joinRegistered` covers this lane.
    const {
        reconnectRegisteredClone: _desktopReconnectRegisteredClone,
        ...androidShape
    } = desktopShape
    let pendingBridgeStop: AndroidDeviceSyncForegroundIdentity | undefined

    const stopPendingBridge = (): void => {
        if (!pendingBridgeStop) return
        try {
            if (!bridge.stopSource(
                pendingBridgeStop.lane,
                pendingBridgeStop.operationId,
                pendingBridgeStop.generation,
            )) throw new DeviceSyncError('cleanup-failed')
        } catch {
            throw new DeviceSyncError('cleanup-failed')
        }
        pendingBridgeStop = undefined
    }

    return {
        ...androidShape,
        async prepare(settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus> {
            if (settings.method !== 'lan') {
                throw new DeviceSyncError('invalid-configuration')
            }
            return desktopShape.prepare({ ...settings, publicBaseUrl: '' })
        },
        async start(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> {
            stopPendingBridge()
            const foregroundInvoke: PeerSyncInvoke = async <T>(
                command: string,
                args?: Record<string, unknown>,
            ): Promise<T> => {
                const result = await nativeInvoke(command, args)
                if (command === 'peer_sync_foreground_source_status') {
                    return safeForegroundIdentity(result) as T
                }
                if (command === 'device_sync_source_reserve') {
                    const identity = safeForegroundIdentity(result)
                    if (!identity) throw new DeviceSyncError('state-unavailable')
                    return identity as T
                }
                if (command === 'device_sync_start') return safeDeviceSyncStatus(result) as T
                return result as T
            }
            const lifecycle = createPeerAndroidSourceForeground<
                AndroidDeviceSyncForegroundIdentity,
                DeviceSyncStatus
            >({
                invoke: foregroundInvoke,
                bridge,
                lane: 'device-sync-source',
                reserveCommand: 'device_sync_source_reserve',
                startCommand: 'device_sync_start',
                startArgs: (_unused, foreground) => ({ permissions, foreground }),
                staleIdentityError: 'state-unavailable',
                serviceStartError: 'transport-unavailable',
                serviceStopError: 'cleanup-failed',
                cleanupFailureMessage: 'device sync start cleanup failed',
            })
            try {
                return (await lifecycle.start('')).result
            } catch (error) {
                if (error instanceof AggregateError) throw error
                throw classifyDeviceSyncFailure(error)
            }
        },
        async stop(): Promise<void> {
            if (pendingBridgeStop) {
                stopPendingBridge()
                return
            }
            let value: unknown
            try {
                value = await nativeInvoke('device_sync_stop')
            } catch (error) {
                throw classifyDeviceSyncFailure(error)
            }
            const foreground = safeForegroundIdentity(value)
            if (!foreground) return
            pendingBridgeStop = foreground
            stopPendingBridge()
        },
    }
}
