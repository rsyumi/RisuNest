import { invoke } from '@tauri-apps/api/core'

import { parsePeerCloneUri, type PeerClonePairing } from './peerClone'

export interface AndroidPeerCloneInvoke {
    <T>(command: string, args?: Record<string, unknown>): Promise<T>
}

export interface AndroidPeerCloneBridge {
    transferMode(): 'foreground' | 'uidt' | 'disabled'
    schedule(jobId: string): 'scheduled' | 'disabled' | 'rejected'
    cancel(jobId: string): boolean
}

export interface AndroidPeerCloneReplacementRuntime {
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

export interface AndroidPeerCloneCapabilities {
    androidClient: boolean
    atomicActivationReady: boolean
    losslessBackupReady: boolean
    httpTransportReady: boolean
    productionEnabled: boolean
}

export interface AndroidPeerCloneTargetStatus {
    jobId: string
    endpoint: string
    sessionId: string
    manifestId: string
    phase: 'ready' | 'paused' | 'downloading' | 'awaitingActivation' | 'cancelled' | 'completed' | 'failed'
    completedBytes: number
    totalBytes?: number
    error?: string
}

export interface AndroidPeerCloneState {
    phase: 'idle' | 'joined' | 'confirmed' | 'paused' | 'downloading' | 'cancelled' | 'completed' | 'failed'
    destructiveConfirmed: boolean
    completedBytes: number
    totalBytes?: number
    error?: string
}

interface AndroidPeerCloneFacadeOptions {
    invoke?: AndroidPeerCloneInvoke
    bridge?: AndroidPeerCloneBridge
    runtime: AndroidPeerCloneReplacementRuntime
}

const initialState: AndroidPeerCloneState = {
    phase: 'idle',
    destructiveConfirmed: false,
    completedBytes: 0,
}

function browserBridge(): AndroidPeerCloneBridge {
    const bridge = (window as Window & { RisuPeerCloneBridge?: AndroidPeerCloneBridge }).RisuPeerCloneBridge
    if (!bridge) throw new Error('Android peer clone platform bridge is unavailable')
    return bridge
}

export function createAndroidPeerCloneFacade(options: AndroidPeerCloneFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    const platformBridge = options.bridge ?? browserBridge()
    let state = initialState
    let pairing: PeerClonePairing | undefined
    let jobId: string | undefined
    let finalization: Promise<AndroidPeerCloneTargetStatus> | undefined

    const capabilities = () => nativeInvoke<AndroidPeerCloneCapabilities>('peer_clone_android_capabilities')
    const requireReady = async () => {
        const current = await capabilities()
        if (
            !current.androidClient
            || !current.atomicActivationReady
            || !current.losslessBackupReady
            || !current.httpTransportReady
            || !current.productionEnabled
        ) throw new Error('Android peer clone is not enabled by native production gates')
    }
    const beginTransfer = async (id: string): Promise<void> => {
        const mode = platformBridge.transferMode()
        if (mode === 'uidt') {
            if (platformBridge.schedule(id) !== 'scheduled') {
                throw new Error('Android user-initiated clone transfer was not scheduled')
            }
        } else if (mode === 'foreground') {
            void nativeInvoke('peer_clone_android_download', { jobId: id }).catch((cause) => {
                state = {
                    ...state,
                    phase: 'failed',
                    error: cause instanceof Error ? cause.message : String(cause),
                }
            })
        } else {
            throw new Error('Android peer clone transfer is disabled')
        }
        state = { ...state, phase: 'downloading' }
    }
    const adoptStatus = (status: AndroidPeerCloneTargetStatus | null): AndroidPeerCloneTargetStatus | null => {
        if (!status) {
            jobId = undefined
            state = initialState
            return null
        }
        jobId = status.jobId
        pairing = {
            endpoint: status.endpoint,
            sessionId: status.sessionId,
            manifestId: status.manifestId,
            claim: '',
        }
        const phase = status.phase === 'ready' || status.phase === 'paused'
            ? 'paused'
            : status.phase === 'awaitingActivation'
                ? 'downloading'
                : status.phase
        state = {
            phase,
            destructiveConfirmed: true,
            completedBytes: status.completedBytes,
            totalBytes: status.totalBytes,
            error: status.error,
        }
        return status
    }
    const finalize = (status: AndroidPeerCloneTargetStatus): Promise<AndroidPeerCloneTargetStatus> => {
        finalization ??= (async () => {
            const id = jobId
            if (!id) throw new Error('Android peer clone job is unavailable')
            const token = await options.runtime.capturePersistentMutationToken('peer-clone-target-finalize')
            const fence = await options.runtime.acquireDestructiveReplacementFence(token)
            try {
                const result = await nativeInvoke<{ revision: number }>('peer_clone_android_finalize', {
                    jobId: id,
                    expectedRevision: token.revision,
                })
                await fence.refreshCommittedWorkingSet(result.revision)
                const completed = { ...status, phase: 'completed' as const }
                state = { ...state, phase: 'completed' }
                return completed
            } catch (cause) {
                state = {
                    ...state,
                    phase: 'failed',
                    error: cause instanceof Error ? cause.message : String(cause),
                }
                throw cause
            } finally {
                fence.release()
                finalization = undefined
            }
        })()
        return finalization
    }

    return {
        getState: () => state,
        capabilities,
        join(uri: string): AndroidPeerCloneState {
            pairing = parsePeerCloneUri(uri)
            jobId = undefined
            state = { ...initialState, phase: 'joined' }
            return state
        },
        confirmDestructiveReplace(): AndroidPeerCloneState {
            if (!pairing) throw new Error('Android peer clone target has not joined a pairing')
            state = { ...state, phase: 'confirmed', destructiveConfirmed: true }
            return state
        },
        async download(): Promise<void> {
            if (!state.destructiveConfirmed || !pairing) {
                throw new Error('Android peer clone target requires destructive replacement confirmation')
            }
            await requireReady()
            const claimed = await nativeInvoke<AndroidPeerCloneTargetStatus>('peer_clone_android_claim', {
                endpoint: pairing.endpoint,
                sessionId: pairing.sessionId,
                manifestId: pairing.manifestId,
                claim: pairing.claim,
            })
            adoptStatus(claimed)
            await beginTransfer(claimed.jobId)
        },
        async recover(): Promise<AndroidPeerCloneTargetStatus | null> {
            const current = await nativeInvoke<AndroidPeerCloneTargetStatus | null>('peer_clone_android_current')
            return adoptStatus(current)
        },
        async resume(): Promise<void> {
            if (!jobId) throw new Error('Android peer clone job is unavailable')
            await requireReady()
            await beginTransfer(jobId)
        },
        async cancel(): Promise<void> {
            if (!jobId) throw new Error('Android peer clone job is unavailable')
            await nativeInvoke('peer_clone_android_request_cancel', { jobId })
            const mode = platformBridge.transferMode()
            if (mode === 'uidt') {
                if (!platformBridge.cancel(jobId)) throw new Error('Android peer clone cancellation failed')
            } else {
                await nativeInvoke('peer_clone_android_cancel_foreground', { jobId })
            }
            state = { ...state, phase: 'cancelled' }
        },
        async targetStatus(): Promise<AndroidPeerCloneTargetStatus> {
            const current = await nativeInvoke<AndroidPeerCloneTargetStatus | null>('peer_clone_android_current')
            const status = adoptStatus(current)
            if (!status) throw new Error('Android peer clone job is unavailable')
            return status.phase === 'awaitingActivation' ? finalize(status) : status
        },
    }
}
