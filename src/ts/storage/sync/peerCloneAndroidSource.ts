import { invoke } from '@tauri-apps/api/core'

import type { PeerCloneInvoke, PeerCloneSourceStatus } from './peerClone'
import { createPeerAndroidSourceForeground } from './peerAndroidSourceForeground'
import type { PeerSyncForegroundBridge } from './peerSyncShared'

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

export type AndroidPeerCloneSourceBridge = PeerSyncForegroundBridge

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
    const p1SourceForeground = createPeerAndroidSourceForeground<
        AndroidPeerCloneForegroundIdentity,
        PeerCloneSourceStatus
    >({
        invoke: nativeInvoke,
        bridge,
        lane: 'p1-source',
        reserveCommand: 'peer_clone_android_source_reserve',
        startCommand: 'peer_clone_android_source_start',
        startArgs: (sessionId, identity) => ({ sessionId, foreground: identity }),
        staleIdentityError: 'Android peer clone foreground identity is stale',
        serviceStartError: 'Android peer clone foreground service could not start',
        serviceStopError: 'Android peer clone foreground service could not stop',
        cleanupFailureMessage: 'Android peer clone foreground start and cleanup both failed',
    })

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
            const started = await p1SourceForeground.start(sessionId)
            foreground = started.foreground
            return started.result
        },
        async stop(sessionId: string): Promise<void> {
            const identity = await nativeInvoke<AndroidPeerCloneForegroundIdentity | null>(
                'peer_clone_android_source_stop',
                { sessionId },
            )
            if (identity && !bridge.stopSource(identity.lane, identity.operationId, identity.generation)) {
                throw new Error('Android peer clone foreground service could not stop')
            }
            foreground = undefined
        },
        revoke(sessionId: string, deviceId: string): Promise<void> {
            return nativeInvoke('peer_clone_android_source_revoke', { sessionId, deviceId })
        },
    }
}
