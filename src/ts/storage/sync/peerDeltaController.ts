import {
    createPeerDeltaFacade,
    type PeerDeltaCapabilities,
    type PeerDeltaMutationRuntime,
    type PeerDeltaPullResult,
    type RetainedDeltaCompletion,
} from './peerDelta'

type PeerDeltaFacade = ReturnType<typeof createPeerDeltaFacade>

export interface PeerDeltaControllerSnapshot {
    capabilities?: PeerDeltaCapabilities
    pullPhase: 'idle' | 'running' | 'completed' | 'fullCloneRequired' | 'conflict' | 'failed'
    pullResult?: PeerDeltaPullResult
    retained: RetainedDeltaCompletion | null
    error: string
}

export function createPeerDeltaController(options: { facade: PeerDeltaFacade }) {
    const listeners = new Set<(snapshot: PeerDeltaControllerSnapshot) => void>()
    let snapshot: PeerDeltaControllerSnapshot = { pullPhase: 'idle', retained: null, error: '' }
    let initialized = false
    let initialization: Promise<void> | undefined
    let activePull: Promise<PeerDeltaPullResult> | undefined

    const publish = (): void => {
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<PeerDeltaControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        publish()
    }
    const message = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))
    // The retained completion journal only changes when a pull ends, so it is
    // read there and at initialization rather than on a timer.
    const refreshRetainedAfterPull = async (): Promise<void> => {
        try {
            update({ retained: await options.facade.retained() })
        } catch {
            // The pull outcome stands on its own. The refusal wording covers the
            // moment the retained state could not be read.
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
            initialization = options.facade.recoverTargetForeground().then(
                () => Promise.all([options.facade.capabilities(), options.facade.retained()]),
            ).then(([capabilities, retained]) => {
                update({ capabilities, retained, error: '' })
            }).catch((cause) => {
                initialized = false
                initialization = undefined
                update({ error: message(cause) })
            })
            return initialization
        },
        pullRegistered(deviceId: string): Promise<PeerDeltaPullResult> {
            if (activePull) return Promise.reject(new Error('A peer delta pull is already running'))
            update({ pullPhase: 'running', pullResult: undefined, error: '' })
            const promise = options.facade.pullRegistered(deviceId).then((pullResult) => {
                update({
                    pullResult,
                    pullPhase: pullResult.kind === 'fullCloneRequired'
                        ? 'fullCloneRequired'
                        : pullResult.kind === 'conflict' ? 'conflict' : 'completed',
                })
                return pullResult
            }).catch((cause) => {
                update({ pullPhase: 'failed', error: message(cause) })
                throw cause
            }).finally(async () => {
                // The refresh runs before the fence drops, so a pull that starts
                // during it meets this controller's own refusal instead of the
                // native target guard.
                await refreshRetainedAfterPull()
                activePull = undefined
            })
            activePull = promise
            return promise
        },
        async abandonRetained(): Promise<void> {
            const retained = snapshot.retained
            if (!retained) throw new Error('No retained peer delta completion to abandon')
            try {
                await options.facade.abandonRetained(retained.operationId)
            } catch (cause) {
                update({ pullPhase: 'failed', error: message(cause) })
                throw cause
            }
            let refreshed: RetainedDeltaCompletion | null = null
            try {
                refreshed = await options.facade.retained()
            } catch {
                // The abandonment already stands and the journal it named is
                // gone, so a re-read that cannot answer must not put the target
                // back on the state it just left.
            }
            update({ retained: refreshed, pullPhase: 'idle', pullResult: undefined, error: '' })
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

let androidPeerDeltaController: ReturnType<typeof createPeerDeltaController> | undefined

export function getAndroidPeerDeltaController(runtime: PeerDeltaMutationRuntime) {
    androidPeerDeltaController ??= createPeerDeltaController({
        facade: createPeerDeltaFacade({ platform: 'android', runtime }),
    })
    return androidPeerDeltaController
}
