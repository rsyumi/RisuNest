import { invoke } from '@tauri-apps/api/core'

import type { PeerCloneInvoke, PeerCloneSourceStatus } from './peerClone'

export interface AndroidPeerCloneSourceCapabilities {
    desktop: false
    sourceReady: boolean
    atomicActivationReady: false
    losslessBackupReady: boolean
    httpTransportReady: boolean
    largeFixturePassed: boolean
    productionEnabled: boolean
    tunnelReady: false
}

export interface AndroidPeerCloneForegroundIdentity {
    lane: 'p1-source'
    operationId: string
    generation: number
}

export interface AndroidPeerCloneSourceBridge {
    startSource(lane: string, operationId: string, generation: number): boolean
    stopSource(lane: string, operationId: string, generation: number): boolean
}

export interface AndroidPeerCloneSourceFacadeOptions {
    invoke?: PeerCloneInvoke
    bridge?: AndroidPeerCloneSourceBridge
    flushPendingData(reason: string): Promise<void>
}

declare global {
    interface Window {
        RisuPeerCloneBridge?: AndroidPeerCloneSourceBridge & {
            transferMode(): 'foreground' | 'uidt' | 'disabled'
            schedule(jobId: string): 'scheduled' | 'disabled' | 'rejected'
            cancel(jobId: string): boolean
        }
    }
}

function nativeBridge(): AndroidPeerCloneSourceBridge {
    const bridge = window.RisuPeerCloneBridge
    if (!bridge) throw new Error('Android peer clone foreground service is unavailable')
    return bridge
}

export function createAndroidPeerCloneSourceFacade(options: AndroidPeerCloneSourceFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    const bridge = options.bridge ?? nativeBridge()
    let foreground: AndroidPeerCloneForegroundIdentity | undefined

    return {
        foregroundIdentity: () => foreground,
        capabilities(): Promise<AndroidPeerCloneSourceCapabilities> {
            return nativeInvoke('peer_clone_android_source_capabilities')
        },
        async prepare(): Promise<PeerCloneSourceStatus> {
            await options.flushPendingData('peer-clone-android-source-prepare')
            return nativeInvoke('peer_clone_android_source_prepare')
        },
        status(): Promise<PeerCloneSourceStatus> {
            return nativeInvoke('peer_clone_android_source_status')
        },
        async start(sessionId: string): Promise<PeerCloneSourceStatus> {
            const identity = await nativeInvoke<AndroidPeerCloneForegroundIdentity>(
                'peer_clone_android_source_reserve',
            )
            if (!bridge.startSource(identity.lane, identity.operationId, identity.generation)) {
                throw new Error('Android peer clone foreground service could not start')
            }
            try {
                const status = await nativeInvoke<PeerCloneSourceStatus>('peer_clone_android_source_start', {
                    sessionId,
                    foreground: identity,
                })
                foreground = identity
                return status
            } catch (error) {
                bridge.stopSource(identity.lane, identity.operationId, identity.generation)
                throw error
            }
        },
        async stop(sessionId: string): Promise<void> {
            const identity = await nativeInvoke<AndroidPeerCloneForegroundIdentity | null>(
                'peer_clone_android_source_stop',
                { sessionId },
            )
            if (identity) bridge.stopSource(identity.lane, identity.operationId, identity.generation)
            foreground = undefined
        },
        revoke(sessionId: string, deviceId: string): Promise<void> {
            return nativeInvoke('peer_clone_android_source_revoke', { sessionId, deviceId })
        },
    }
}
