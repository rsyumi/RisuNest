import {
    createPeerCloneFacade,
    initialPeerCloneState,
    type PeerCloneNativeCapabilities,
    type PeerCloneReplacementRuntime,
    type PeerCloneState,
    type PeerCloneTargetStatus,
} from './peerClone'

type PeerCloneFacade = ReturnType<typeof createPeerCloneFacade>

export interface PeerCloneControllerSnapshot {
    capabilities?: PeerCloneNativeCapabilities
    state: PeerCloneState
    targetPhase?: PeerCloneTargetStatus['phase']
    error: string
    warning: string
}

export interface PeerCloneControllerOptions {
    facade: PeerCloneFacade
    targetPollMilliseconds?: number
}

export function createPeerCloneController(options: PeerCloneControllerOptions) {
    const facade = options.facade
    const listeners = new Set<(snapshot: PeerCloneControllerSnapshot) => void>()
    let snapshot: PeerCloneControllerSnapshot = {
        state: initialPeerCloneState,
        error: '',
        warning: '',
    }
    let initialized = false
    let initialization: Promise<void> | undefined
    let targetTimer: ReturnType<typeof setInterval> | undefined
    let targetPolling = false

    const publish = () => {
        snapshot = {
            ...snapshot,
            state: facade.getState(),
            warning: facade.getWarning(),
        }
        for (const listener of listeners) listener(snapshot)
    }
    const failure = (cause: unknown) => {
        snapshot = { ...snapshot, error: cause instanceof Error ? cause.message : String(cause) }
        publish()
    }
    const success = () => {
        snapshot = { ...snapshot, error: '' }
        publish()
    }
    const stopTargetPolling = () => {
        if (targetTimer) clearInterval(targetTimer)
        targetTimer = undefined
    }
    const pollTarget = async () => {
        if (targetPolling) return
        targetPolling = true
        try {
            const status = await facade.targetStatus()
            snapshot = { ...snapshot, targetPhase: status.phase }
            const phase = facade.getState().target.phase
            if (phase === 'failed') {
                snapshot = { ...snapshot, error: status.error ?? 'Peer clone target failed' }
                publish()
            } else {
                success()
            }
            if (phase === 'completed' || phase === 'failed') stopTargetPolling()
        } catch (cause) {
            failure(cause)
            if (facade.getState().target.phase === 'failed') stopTargetPolling()
        } finally {
            targetPolling = false
        }
    }
    const beginTargetPolling = () => {
        if (!targetTimer) {
            targetTimer = setInterval(() => void pollTarget(), options.targetPollMilliseconds ?? 500)
        }
    }
    const run = async <T>(operation: () => Promise<T>): Promise<T> => {
        try {
            const result = await operation()
            success()
            return result
        } catch (cause) {
            failure(cause)
            throw cause
        }
    }

    return {
        snapshot: () => snapshot,
        subscribe(listener: (value: PeerCloneControllerSnapshot) => void): () => void {
            listeners.add(listener)
            listener(snapshot)
            return () => listeners.delete(listener)
        },
        initialize(): Promise<void> {
            if (initialized) return initialization ?? Promise.resolve()
            initialized = true
            initialization = facade.capabilities().then((capabilities) => {
                snapshot = { ...snapshot, capabilities }
                success()
            }).catch((cause) => {
                initialized = false
                initialization = undefined
                failure(cause)
            })
            return initialization
        },
        joinClaimed(target: { endpoint: string, sessionId: string, manifestId: string }): void {
            facade.joinClaimed(target)
            snapshot = { ...snapshot, targetPhase: 'idle' }
            success()
        },
        confirmDestructiveReplace(): void {
            facade.confirmDestructiveReplace()
            success()
        },
        download: () => run(async () => {
            await facade.download()
            snapshot = { ...snapshot, targetPhase: 'downloading' }
            beginTargetPolling()
        }),
        resume: () => run(async () => {
            await facade.resume()
            snapshot = { ...snapshot, targetPhase: 'downloading' }
            beginTargetPolling()
        }),
        cancel: () => run(async () => {
            await facade.cancel()
            snapshot = { ...snapshot, targetPhase: 'cancelled' }
            stopTargetPolling()
        }),
    }
}

let desktopPeerCloneController: ReturnType<typeof createPeerCloneController> | undefined

export function getDesktopPeerCloneController(runtime: PeerCloneReplacementRuntime) {
    desktopPeerCloneController ??= createPeerCloneController({
        facade: createPeerCloneFacade({
            platform: 'desktop',
            runtime,
        }),
    })
    return desktopPeerCloneController
}
