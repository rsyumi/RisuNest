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
    let sourceError = ''
    let pullError = ''
    let activePull: { pairingUri: string, promise: Promise<PeerDeltaPullResult> } | undefined

    const publish = (): void => {
        snapshot = { ...snapshot, error: pullError || sourceError }
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
            sourceError = ''
            update({
                sourceStatus,
                sourcePairingUri: sourceStatus.phase === 'running'
                    ? sourceStatus.pairingUri ?? snapshot.sourcePairingUri
                    : '',
            })
            if (sourceStatus.phase !== 'running') stopSourcePolling()
        } catch (cause) {
            sourceError = cause instanceof Error ? cause.message : String(cause)
            publish()
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
            sourceError = ''
            publish()
            return result
        } catch (cause) {
            sourceError = cause instanceof Error ? cause.message : String(cause)
            publish()
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
                sourceError = ''
                update({
                    capabilities,
                    sourceStatus,
                    sourcePairingUri: sourceStatus.phase === 'running'
                        ? sourceStatus.pairingUri ?? ''
                        : '',
                })
                if (sourceStatus.phase === 'running') beginSourcePolling()
            }).catch((cause) => {
                sourceError = cause instanceof Error ? cause.message : String(cause)
                initialized = false
                initialization = undefined
                publish()
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
            if (activePull) {
                if (activePull.pairingUri !== pairingUri) {
                    return Promise.reject(new Error('A peer delta pull for a different pairing is already running'))
                }
                return activePull.promise
            }
            pullError = ''
            update({ pullPhase: 'running', pullResult: undefined })
            const promise = options.facade.pull(pairingUri).then((pullResult) => {
                pullError = ''
                update({
                    pullResult,
                    pullPhase: pullResult.kind === 'fullCloneRequired'
                        ? 'fullCloneRequired'
                        : pullResult.kind === 'conflict' ? 'conflict' : 'completed',
                })
                return pullResult
            }).catch((cause) => {
                pullError = cause instanceof Error ? cause.message : String(cause)
                update({
                    pullPhase: 'failed',
                })
                throw cause
            }).finally(() => {
                activePull = undefined
            })
            activePull = { pairingUri, promise }
            return promise
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
