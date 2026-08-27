import {
    createPeerDeltaFacade,
    type PeerDeltaCapabilities,
    type PeerDeltaMutationRuntime,
    type PeerDeltaPullResult,
    type PeerDeltaSourceStatus,
} from './peerDelta'

type PeerDeltaFacade = ReturnType<typeof createPeerDeltaFacade>

export interface PeerDeltaControllerSnapshot {
    capabilities?: PeerDeltaCapabilities
    sourceStatus: PeerDeltaSourceStatus
    sourcePairingUri: string
    pullPhase: 'idle' | 'running' | 'completed' | 'fullCloneRequired' | 'conflict' | 'failed'
    pullResult?: PeerDeltaPullResult
    error: string
}

export function createPeerDeltaController(options: {
    facade: PeerDeltaFacade
    sourcePollMilliseconds?: number
}) {
    const listeners = new Set<(snapshot: PeerDeltaControllerSnapshot) => void>()
    let snapshot: PeerDeltaControllerSnapshot = {
        sourceStatus: { phase: 'idle', devices: [] },
        sourcePairingUri: '',
        pullPhase: 'idle',
        error: '',
    }
    let initialized = false
    let initialization: Promise<void> | undefined
    let sourceTimer: ReturnType<typeof setInterval> | undefined
    let sourcePolling = false
    let activePull: Promise<PeerDeltaPullResult> | undefined

    const publish = (): void => {
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<PeerDeltaControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        publish()
    }
    const stopSourcePolling = (): void => {
        if (sourceTimer) clearInterval(sourceTimer)
        sourceTimer = undefined
    }
    const pollSource = async (): Promise<void> => {
        if (sourcePolling) return
        sourcePolling = true
        try {
            const sourceStatus = await options.facade.status()
            update({
                sourceStatus,
                sourcePairingUri: sourceStatus.phase === 'running'
                    ? snapshot.sourcePairingUri
                    : '',
                error: '',
            })
            if (sourceStatus.phase !== 'running') stopSourcePolling()
        } catch (cause) {
            update({ error: cause instanceof Error ? cause.message : String(cause) })
        } finally {
            sourcePolling = false
        }
    }
    const beginSourcePolling = (): void => {
        if (!sourceTimer) {
            sourceTimer = setInterval(
                () => void pollSource(),
                options.sourcePollMilliseconds ?? 1_000,
            )
        }
    }
    const run = async <T>(operation: () => Promise<T>): Promise<T> => {
        try {
            const result = await operation()
            update({ error: '' })
            return result
        } catch (cause) {
            update({ error: cause instanceof Error ? cause.message : String(cause) })
            throw cause
        }
    }

    return {
        snapshot: (): PeerDeltaControllerSnapshot => snapshot,
        subscribe(listener: (value: PeerDeltaControllerSnapshot) => void): () => void {
            listeners.add(listener)
            listener(snapshot)
            return () => listeners.delete(listener)
        },
        initialize(): Promise<void> {
            if (initialized) return initialization ?? Promise.resolve()
            initialized = true
            initialization = Promise.all([
                options.facade.capabilities(),
                options.facade.status(),
            ]).then(([capabilities, sourceStatus]) => {
                update({ capabilities, sourceStatus, error: '' })
                if (sourceStatus.phase === 'running') beginSourcePolling()
            }).catch((cause) => {
                update({ error: cause instanceof Error ? cause.message : String(cause) })
            })
            return initialization
        },
        prepare: () => run(async () => {
            const sourceStatus = await options.facade.prepare()
            update({ sourceStatus, sourcePairingUri: '' })
            return sourceStatus
        }),
        start: (sessionId: string) => run(async () => {
            const sourceStatus = await options.facade.start(sessionId)
            update({
                sourceStatus,
                sourcePairingUri: sourceStatus.pairingUri ?? '',
            })
            beginSourcePolling()
            return sourceStatus
        }),
        stop: (sessionId: string) => run(async () => {
            await options.facade.stop(sessionId)
            stopSourcePolling()
            const sourceStatus = await options.facade.status()
            update({ sourceStatus, sourcePairingUri: '' })
        }),
        revoke: (sessionId: string, deviceId: string) => run(async () => {
            await options.facade.revoke(sessionId, deviceId)
            update({ sourceStatus: await options.facade.status() })
        }),
        pull(pairingUri: string): Promise<PeerDeltaPullResult> {
            if (activePull) return activePull
            update({ pullPhase: 'running', pullResult: undefined, error: '' })
            activePull = options.facade.pull(pairingUri).then((pullResult) => {
                update({
                    pullResult,
                    pullPhase: pullResult.kind === 'fullCloneRequired'
                        ? 'fullCloneRequired'
                        : pullResult.kind === 'conflict' ? 'conflict' : 'completed',
                    error: '',
                })
                return pullResult
            }).catch((cause) => {
                update({
                    pullPhase: 'failed',
                    error: cause instanceof Error ? cause.message : String(cause),
                })
                throw cause
            }).finally(() => {
                activePull = undefined
            })
            return activePull
        },
    }
}

let desktopPeerDeltaController: ReturnType<typeof createPeerDeltaController> | undefined

export function getDesktopPeerDeltaController(runtime: PeerDeltaMutationRuntime) {
    desktopPeerDeltaController ??= createPeerDeltaController({
        facade: createPeerDeltaFacade({ platform: 'desktop', runtime }),
    })
    return desktopPeerDeltaController
}
