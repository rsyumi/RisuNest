import {
    createPeerCloneFacade,
    initialPeerCloneState,
    type PeerCloneNativeCapabilities,
    type PeerCloneReplacementRuntime,
    type PeerCloneSourceStatus,
    type PeerCloneState,
    type PeerCloneTargetStatus,
    type PeerCloneTunnelStatus,
} from './peerClone'
import { createPeerSourcePolling } from './peerSourcePolling'

type PeerCloneFacade = ReturnType<typeof createPeerCloneFacade>

export interface PeerCloneControllerSnapshot {
    capabilities?: PeerCloneNativeCapabilities
    sourceStatus: PeerCloneSourceStatus
    tunnelStatus: PeerCloneTunnelStatus
    state: PeerCloneState
    targetPhase?: PeerCloneTargetStatus['phase']
    sourcePairingUri: string
    error: string
    warning: string
}

export interface PeerCloneControllerOptions {
    facade: PeerCloneFacade
    targetPollMilliseconds?: number
    sourcePollMilliseconds?: number
}

export function createPeerCloneController(options: PeerCloneControllerOptions) {
    const facade = options.facade
    const listeners = new Set<(snapshot: PeerCloneControllerSnapshot) => void>()
    let snapshot: PeerCloneControllerSnapshot = {
        sourceStatus: { phase: 'idle', devices: [] },
        tunnelStatus: { phase: 'idle' },
        state: initialPeerCloneState,
        sourcePairingUri: '',
        error: '',
        warning: '',
    }
    let initialized = false
    let initialization: Promise<void> | undefined
    let targetInitialized = false
    let targetInitialization: Promise<void> | undefined
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
    const refreshSourceState = async (): Promise<PeerCloneSourceStatus> => {
        const sourceStatus = await facade.sourceStatus()
        const tunnelStatus = sourceStatus.tunnel
            ? await facade.tunnelStatus()
            : { phase: 'idle' as const }
        snapshot = { ...snapshot, sourceStatus, tunnelStatus }
        return sourceStatus
    }
    const sourcePolling = createPeerSourcePolling({
        intervalMilliseconds: options.sourcePollMilliseconds ?? 1_000,
        poll: async (): Promise<void> => {
        try {
            const sourceStatus = await refreshSourceState()
            snapshot = { ...snapshot, error: '' }
            if (sourceStatus.phase !== 'running') {
                snapshot = { ...snapshot, sourcePairingUri: '' }
            }
            if (sourceStatus.phase !== 'starting' && sourceStatus.phase !== 'running' && sourceStatus.phase !== 'stopping') {
                sourcePolling.stop()
            }
            publish()
        } catch (cause) {
            failure(cause)
        }
        },
    })
    const stopSourcePolling = (): void => sourcePolling.stop()
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
    const beginSourcePolling = (): void => sourcePolling.start()
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
    const startTunnel = (start: () => Promise<PeerCloneSourceStatus>) => run(async () => {
        try {
            const sourceStatus = await start()
            const tunnelStatus = await facade.tunnelStatus()
            snapshot = {
                ...snapshot,
                sourceStatus,
                tunnelStatus,
                sourcePairingUri: sourceStatus.pairingUri ?? '',
            }
            beginSourcePolling()
            return sourceStatus
        } catch (cause) {
            try {
                const sourceStatus = await refreshSourceState()
                snapshot = { ...snapshot, sourcePairingUri: '' }
                if (sourceStatus.phase === 'starting' || sourceStatus.phase === 'running' || sourceStatus.phase === 'stopping') {
                    beginSourcePolling()
                }
            } catch {
                snapshot = { ...snapshot, sourcePairingUri: '' }
            }
            throw cause
        }
    })

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
            initialization = Promise.all([
                facade.capabilities().then((capabilities) => {
                    snapshot = { ...snapshot, capabilities }
                }),
                refreshSourceState().then((sourceStatus) => {
                    if (sourceStatus.phase === 'starting' || sourceStatus.phase === 'running' || sourceStatus.phase === 'stopping') {
                        beginSourcePolling()
                    }
                }),
            ]).then(() => success()).catch((cause) => {
                initialized = false
                initialization = undefined
                failure(cause)
            })
            return initialization
        },
        initializeTarget(): Promise<void> {
            if (targetInitialized) return targetInitialization ?? Promise.resolve()
            targetInitialized = true
            targetInitialization = facade.capabilities().then((capabilities) => {
                snapshot = { ...snapshot, capabilities }
                success()
            }).catch((cause) => {
                targetInitialized = false
                targetInitialization = undefined
                failure(cause)
            })
            return targetInitialization
        },
        clearError(): void {
            success()
        },
        join(pairingUri: string): void {
            facade.join(pairingUri)
            snapshot = { ...snapshot, targetPhase: 'idle' }
            success()
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
        prepare: () => run(async () => {
            const sourceStatus = await facade.prepare()
            snapshot = { ...snapshot, sourceStatus }
            return sourceStatus
        }),
        start: (sessionId: string) => run(async () => {
            const sourceStatus = await facade.start(sessionId)
            snapshot = {
                ...snapshot,
                sourceStatus,
                sourcePairingUri: sourceStatus.pairingUri ?? '',
            }
            beginSourcePolling()
            return sourceStatus
        }),
        startQuickTunnel: (sessionId: string) => startTunnel(() => facade.startQuickTunnel(sessionId)),
        startNamedTunnel: (
            sessionId: string,
            token: string,
            expectedPublicBaseUrl: string,
        ) => startTunnel(() => facade.startNamedTunnel(sessionId, token, expectedPublicBaseUrl)),
        stop: (sessionId: string) => run(async () => {
            try {
                if (snapshot.sourceStatus.tunnel) {
                    await facade.stopTunnel(sessionId)
                } else {
                    await facade.stop(sessionId)
                }
            } finally {
                const sourceStatus = await refreshSourceState()
                if (sourceStatus.phase === 'starting' || sourceStatus.phase === 'running' || sourceStatus.phase === 'stopping') {
                    beginSourcePolling()
                } else {
                    stopSourcePolling()
                }
                if (sourceStatus.phase !== 'running') {
                    snapshot = { ...snapshot, sourcePairingUri: '' }
                }
            }
        }),
        revoke: (sessionId: string, deviceId: string) => run(async () => {
            await facade.revoke(sessionId, deviceId)
            snapshot = { ...snapshot, sourceStatus: await facade.sourceStatus() }
        }),
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
