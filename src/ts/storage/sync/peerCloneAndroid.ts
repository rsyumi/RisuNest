import { invoke } from '@tauri-apps/api/core'

import { NativeFileJobActivationCommittedError } from '../nativeFileJobs'
import { parsePeerCloneEndpoint, parsePeerCloneUri, type PeerClonePairing } from './peerClone'
import type { PeerSyncInvoke, PeerSyncMutationRuntime } from './peerSyncShared'

export type AndroidPeerCloneInvoke = PeerSyncInvoke

export interface AndroidPeerCloneBridge {
    transferMode(): 'foreground' | 'uidt' | 'disabled'
    schedule(jobId: string): 'scheduled' | 'disabled' | 'rejected'
    cancel(jobId: string): boolean
}

export interface AndroidPeerCloneReplacementRuntime extends Omit<PeerSyncMutationRuntime, 'flushPendingData'> {
    afterRefresh?(): void | Promise<void>
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
    committedRevision?: number
    error?: string
}

export interface AndroidRegisteredCloneStatus {
    sourceDeviceId: string
    jobId: string
    phase: AndroidPeerCloneTargetStatus['phase']
    completedBytes: number
    totalBytes?: number
    committedRevision?: number
    error?: string
}

export type AndroidPeerCloneStatus = AndroidPeerCloneTargetStatus | AndroidRegisteredCloneStatus

export interface AndroidPeerCloneState {
    phase: 'idle' | 'joined' | 'confirmed' | 'paused' | 'downloading' | 'cancelled' | 'completed' | 'failed'
    destructiveConfirmed: boolean
    activationCommitted: boolean
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
    activationCommitted: false,
    completedBytes: 0,
}

const canonicalUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const canonicalSourceDeviceUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/
const canonicalUuidV4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
const sha256 = /^[0-9a-f]{64}$/
const statusPhases = new Set<AndroidPeerCloneTargetStatus['phase']>([
    'ready', 'paused', 'downloading', 'awaitingActivation', 'cancelled', 'completed', 'failed',
])

function invalidStatus(): never {
    throw new Error('invalid Android peer clone status')
}

function safeStatusFields(source: Record<string, unknown>): Pick<
    AndroidPeerCloneTargetStatus,
    'jobId' | 'phase' | 'completedBytes' | 'totalBytes' | 'committedRevision' | 'error'
> {
    if (
        typeof source.jobId !== 'string'
        || !canonicalUuidV4.test(source.jobId)
        || typeof source.phase !== 'string'
        || !statusPhases.has(source.phase as AndroidPeerCloneTargetStatus['phase'])
        || typeof source.completedBytes !== 'number'
        || !Number.isSafeInteger(source.completedBytes)
        || source.completedBytes < 0
        || (source.totalBytes !== undefined && (
            typeof source.totalBytes !== 'number'
            || !Number.isSafeInteger(source.totalBytes)
            || source.totalBytes < 0
        ))
        || (source.committedRevision !== undefined && (
            typeof source.committedRevision !== 'number'
            || !Number.isSafeInteger(source.committedRevision)
            || source.committedRevision < 0
        ))
        || (source.error !== undefined && typeof source.error !== 'string')
    ) invalidStatus()
    return {
        jobId: source.jobId,
        phase: source.phase as AndroidPeerCloneTargetStatus['phase'],
        completedBytes: source.completedBytes,
        ...(typeof source.totalBytes === 'number' ? { totalBytes: source.totalBytes } : {}),
        ...(typeof source.committedRevision === 'number' ? { committedRevision: source.committedRevision } : {}),
        ...(typeof source.error === 'string' ? { error: source.error } : {}),
    }
}

function safeRegisteredStatus(value: unknown): AndroidRegisteredCloneStatus {
    if (!value || typeof value !== 'object' || Array.isArray(value)) invalidStatus()
    const source = value as Record<string, unknown>
    const allowed = new Set([
        'sourceDeviceId', 'jobId', 'phase', 'completedBytes', 'totalBytes', 'committedRevision', 'error',
    ])
    if (
        Object.keys(source).some((key) => !allowed.has(key))
        || typeof source.sourceDeviceId !== 'string'
        || !canonicalSourceDeviceUuid.test(source.sourceDeviceId)
        || (source.error !== undefined && source.error !== 'transferFailed')
    ) invalidStatus()
    return { sourceDeviceId: source.sourceDeviceId, ...safeStatusFields(source) }
}

function safeDirectStatus(value: unknown): AndroidPeerCloneTargetStatus {
    if (!value || typeof value !== 'object' || Array.isArray(value)) invalidStatus()
    const source = value as Record<string, unknown>
    const allowed = new Set([
        'jobId', 'endpoint', 'sessionId', 'manifestId', 'phase', 'completedBytes',
        'totalBytes', 'committedRevision', 'error',
    ])
    if (
        Object.keys(source).some((key) => !allowed.has(key))
        || typeof source.endpoint !== 'string'
        || typeof source.sessionId !== 'string'
        || !canonicalUuid.test(source.sessionId)
        || typeof source.manifestId !== 'string'
        || !sha256.test(source.manifestId)
    ) invalidStatus()
    let endpoint: string
    try {
        endpoint = parsePeerCloneEndpoint(source.endpoint)
    } catch {
        invalidStatus()
    }
    return {
        endpoint,
        sessionId: source.sessionId,
        manifestId: source.manifestId,
        ...safeStatusFields(source),
    }
}

function safeCurrentStatus(value: unknown): AndroidPeerCloneStatus | null {
    if (value === null) return null
    if (!value || typeof value !== 'object' || Array.isArray(value)) invalidStatus()
    return Object.prototype.hasOwnProperty.call(value, 'sourceDeviceId')
        ? safeRegisteredStatus(value)
        : safeDirectStatus(value)
}

function browserBridge(): AndroidPeerCloneBridge {
    const bridge = (window as Window & { RisuPeerCloneBridge?: AndroidPeerCloneBridge }).RisuPeerCloneBridge
    return bridge ?? {
        transferMode: () => 'disabled',
        schedule: () => 'disabled',
        cancel: () => false,
    }
}

export function createAndroidPeerCloneFacade(options: AndroidPeerCloneFacadeOptions) {
    const nativeInvoke = options.invoke ?? invoke
    const platformBridge = options.bridge ?? browserBridge()
    let state = initialState
    let pairing: PeerClonePairing | undefined
    let jobId: string | undefined
    let finalization: Promise<AndroidPeerCloneStatus> | undefined
    let committedRecovery: {
        status: AndroidPeerCloneStatus
        committedRevision: number
        refreshRevision: number
        fence: Awaited<ReturnType<AndroidPeerCloneReplacementRuntime['acquireDestructiveReplacementFence']>>
        workingSetRefreshed: boolean
        pluginsRefreshed: boolean
        nativeReleased: boolean
    } | undefined

    const capabilities = async (): Promise<AndroidPeerCloneCapabilities> => {
        const current = await nativeInvoke<AndroidPeerCloneCapabilities>('peer_clone_android_capabilities')
        return platformBridge.transferMode() === 'disabled'
            ? { ...current, productionEnabled: false }
            : current
    }
    const requireReady = async () => {
        const current = await capabilities()
        if (
            !current.androidClient
            || !current.atomicActivationReady
            || !current.losslessBackupReady
            || !current.httpTransportReady
            || !current.productionEnabled
        ) throw new Error('Android peer clone is not enabled by native production gates')
        return platformBridge.transferMode()
    }
    const beginTransfer = async (
        id: string,
        mode: 'foreground' | 'uidt',
    ): Promise<void> => {
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
        }
        state = { ...state, phase: 'downloading' }
    }
    const adoptStatus = (status: AndroidPeerCloneStatus | null): AndroidPeerCloneStatus | null => {
        if (!status) {
            jobId = undefined
            state = initialState
            return null
        }
        jobId = status.jobId
        pairing = 'sourceDeviceId' in status
            ? undefined
            : {
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
            destructiveConfirmed: state.destructiveConfirmed,
            activationCommitted: status.committedRevision !== undefined,
            completedBytes: status.completedBytes,
            totalBytes: status.totalBytes,
            error: status.error,
        }
        return status
    }
    const finishCommittedRecovery = async (): Promise<AndroidPeerCloneStatus> => {
        const recovery = committedRecovery
        if (!recovery) throw new Error('Android peer clone committed recovery is unavailable')
        try {
            if (!recovery.workingSetRefreshed) {
                await recovery.fence.refreshCommittedWorkingSet(recovery.refreshRevision)
                recovery.workingSetRefreshed = true
            }
            if (!recovery.pluginsRefreshed) {
                await options.runtime.afterRefresh?.()
                recovery.pluginsRefreshed = true
            }
            if (!recovery.nativeReleased) {
                await nativeInvoke('peer_clone_android_release', { jobId: recovery.status.jobId })
                recovery.nativeReleased = true
            }
        } catch (cause) {
            const committed = new NativeFileJobActivationCommittedError(recovery.committedRevision, cause)
            state = { ...state, error: committed.message }
            throw committed
        }

        const completed = {
            ...recovery.status,
            phase: 'completed' as const,
            committedRevision: recovery.committedRevision,
        }
        state = { ...state, phase: 'completed', error: undefined }
        recovery.fence.release()
        committedRecovery = undefined
        return completed
    }
    const prepareRestartedRecovery = async (
        status: AndroidPeerCloneStatus & { committedRevision: number },
    ): Promise<void> => {
        try {
            const token = await options.runtime.capturePersistentMutationToken('peer-clone-target-finalize')
            const fence = await options.runtime.acquireDestructiveReplacementFence(token)
            committedRecovery = {
                status,
                committedRevision: status.committedRevision,
                refreshRevision: Math.max(status.committedRevision, token.revision),
                fence,
                workingSetRefreshed: false,
                pluginsRefreshed: false,
                nativeReleased: false,
            }
        } catch (cause) {
            throw new NativeFileJobActivationCommittedError(status.committedRevision, cause)
        }
    }
    const finalize = (status: AndroidPeerCloneStatus): Promise<AndroidPeerCloneStatus> => {
        if (finalization) return finalization
        finalization = (async () => {
            let uncommittedFence: Awaited<ReturnType<AndroidPeerCloneReplacementRuntime['acquireDestructiveReplacementFence']>> | undefined
            try {
                const id = jobId
                if (!id) throw new Error('Android peer clone job is unavailable')
                if (committedRecovery) return await finishCommittedRecovery()
                if (status.committedRevision !== undefined) {
                    await prepareRestartedRecovery(status as AndroidPeerCloneStatus & { committedRevision: number })
                    return await finishCommittedRecovery()
                }

                const token = await options.runtime.capturePersistentMutationToken('peer-clone-target-finalize')
                uncommittedFence = await options.runtime.acquireDestructiveReplacementFence(token)
                const result = await nativeInvoke<{ revision: number }>('peer_clone_android_finalize', {
                    jobId: id,
                    expectedRevision: token.revision,
                })
                committedRecovery = {
                    status: { ...status, committedRevision: result.revision },
                    committedRevision: result.revision,
                    refreshRevision: result.revision,
                    fence: uncommittedFence,
                    workingSetRefreshed: false,
                    pluginsRefreshed: false,
                    nativeReleased: false,
                }
                state = { ...state, activationCommitted: true }
                uncommittedFence = undefined
                return await finishCommittedRecovery()
            } finally {
                uncommittedFence?.release()
                finalization = undefined
            }
        })()
        return finalization
    }

    return {
        getState: () => state,
        capabilities,
        join(uri: string): AndroidPeerCloneState {
            if (jobId && state.phase !== 'cancelled' && state.phase !== 'completed') {
                throw new Error('Android peer clone already owns a clone job')
            }
            pairing = parsePeerCloneUri(uri)
            jobId = undefined
            state = { ...initialState, phase: 'joined' }
            return state
        },
        async joinRegistered(deviceId: string): Promise<AndroidPeerCloneState> {
            if (jobId && state.phase !== 'cancelled' && state.phase !== 'completed') {
                throw new Error('Android peer clone already owns a clone job')
            }
            await requireReady()
            const claimed = safeRegisteredStatus(await nativeInvoke<unknown>(
                'peer_clone_claim_registered_client',
                { deviceId },
            ))
            if (claimed.sourceDeviceId !== deviceId) invalidStatus()
            adoptStatus(claimed)
            state = { ...state, destructiveConfirmed: false }
            return state
        },
        confirmDestructiveReplace(): AndroidPeerCloneState {
            if (!pairing && !jobId) throw new Error('Android peer clone target has not joined a pairing')
            state = {
                ...state,
                phase: state.phase === 'paused' ? 'paused' : 'confirmed',
                destructiveConfirmed: true,
            }
            return state
        },
        async download(): Promise<void> {
            if (!state.destructiveConfirmed || (!pairing && !jobId)) {
                throw new Error('Android peer clone target requires destructive replacement confirmation')
            }
            const request = pairing
            const mode = await requireReady()
            if (mode === 'disabled') throw new Error('Android peer clone is not enabled by native production gates')
            if (jobId) {
                await beginTransfer(jobId, mode)
                return
            }
            if (!request) throw new Error('Android peer clone target has not joined a pairing')
            const claimed = await nativeInvoke<AndroidPeerCloneTargetStatus>('peer_clone_android_claim', {
                endpoint: request.endpoint, sessionId: request.sessionId,
                manifestId: request.manifestId, claim: request.claim,
            })
            adoptStatus(claimed)
            await beginTransfer(claimed.jobId, mode)
        },
        async recover(): Promise<AndroidPeerCloneStatus | null> {
            const current = safeCurrentStatus(await nativeInvoke<unknown>('peer_clone_android_current'))
            return adoptStatus(current)
        },
        async resume(): Promise<void> {
            if (!jobId) throw new Error('Android peer clone job is unavailable')
            if (state.phase !== 'paused') throw new Error('Android peer clone job is not paused')
            if (!state.destructiveConfirmed) {
                throw new Error('Android peer clone target requires destructive replacement confirmation')
            }
            const id = jobId
            const mode = await requireReady()
            if (mode === 'disabled') throw new Error('Android peer clone is not enabled by native production gates')
            await beginTransfer(id, mode)
        },
        async cancel(): Promise<void> {
            if (!jobId) throw new Error('Android peer clone job is unavailable')
            if (state.activationCommitted) throw new Error('Android peer clone activation is already committed')
            const id = jobId
            await nativeInvoke('peer_clone_android_request_cancel', { jobId: id })
            const mode = platformBridge.transferMode()
            if (mode === 'uidt') {
                if (!platformBridge.cancel(id)) throw new Error('Android peer clone cancellation failed')
            } else {
                await nativeInvoke('peer_clone_android_cancel_foreground', { jobId: id })
            }
            state = { ...state, phase: 'cancelled' }
        },
        async targetStatus(): Promise<AndroidPeerCloneStatus> {
            if (committedRecovery) return finalize(committedRecovery.status)
            const current = safeCurrentStatus(await nativeInvoke<unknown>('peer_clone_android_current'))
            const status = adoptStatus(current)
            if (!status) throw new Error('Android peer clone job is unavailable')
            return status.phase === 'awaitingActivation' ? finalize(status) : status
        },
    }
}

let androidPeerCloneFacade: ReturnType<typeof createAndroidPeerCloneFacade> | undefined

export function getAndroidPeerCloneFacade(runtime: AndroidPeerCloneReplacementRuntime) {
    androidPeerCloneFacade ??= createAndroidPeerCloneFacade({ runtime })
    return androidPeerCloneFacade
}
