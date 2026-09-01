import {
    createPeerDeltaFacade,
    type PeerDeltaCapabilities,
    type PeerDeltaMutationRuntime,
    type PeerDeltaPullResult,
    type PeerDeltaSourceStatus,
} from './peerDelta'
import { createPeerSourcePolling } from './peerSourcePolling'
import type { PeerCloneTunnelStatus } from './peerClone'

type PeerDeltaFacade = ReturnType<typeof createPeerDeltaFacade>

export interface PeerDeltaControllerSnapshot {
    capabilities?: PeerDeltaCapabilities
    sourceStatus: PeerDeltaSourceStatus
    tunnelStatus: PeerCloneTunnelStatus
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
        tunnelStatus: { phase: 'idle' },
        sourcePairingUri: '',
        pullPhase: 'idle',
        error: '',
    }
    let initialized = false
    let initialization: Promise<void> | undefined
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
    const refreshSourceState = async (): Promise<PeerDeltaSourceStatus> => {
        const sourceStatus = await options.facade.status()
        const tunnelStatus = sourceStatus.tunnel
            ? await options.facade.tunnelStatus()
            : { phase: 'idle' as const }
        update({
            sourceStatus,
            tunnelStatus,
            sourcePairingUri: sourceStatus.phase === 'running'
                ? sourceStatus.pairingUri ?? snapshot.sourcePairingUri
                : '',
        })
        return sourceStatus
    }
    const sourcePolling = createPeerSourcePolling({
        intervalMilliseconds: options.sourcePollMilliseconds ?? 1_000,
        poll: async (): Promise<void> => {
        try {
            const sourceStatus = await options.facade.status()
            const tunnelStatus = sourceStatus.tunnel
                ? await options.facade.tunnelStatus()
                : { phase: 'idle' as const }
            sourceError = ''
            update({
                sourceStatus,
                tunnelStatus,
                sourcePairingUri: sourceStatus.phase === 'running'
                    ? sourceStatus.pairingUri ?? snapshot.sourcePairingUri
                    : '',
            })
            if (sourceStatus.phase !== 'running') sourcePolling.stop()
        } catch (cause) {
            sourceError = cause instanceof Error ? cause.message : String(cause)
            publish()
        }
        },
    })
    const beginSourcePolling = (): void => sourcePolling.start()
    const stopSourcePolling = (): void => sourcePolling.stop()
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
    const startTunnel = (start: () => Promise<PeerDeltaSourceStatus>) => run(async () => {
        try {
            const sourceStatus = await start()
            update({
                sourceStatus,
                sourcePairingUri: sourceStatus.pairingUri ?? '',
                tunnelStatus: await options.facade.tunnelStatus(),
            })
            beginSourcePolling()
            return sourceStatus
        } catch (cause) {
            const sourceStatus = await refreshSourceState()
            if (['starting', 'running', 'stopping'].includes(sourceStatus.phase)) beginSourcePolling()
            throw cause
        }
    })

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
            initialization = options.facade.recoverTargetForeground().then(() => Promise.all([
                options.facade.capabilities(),
                options.facade.status(),
            ])).then(([capabilities, sourceStatus]) => {
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
        startQuickTunnel: (sessionId: string) => startTunnel(() => options.facade.startQuickTunnel(sessionId)),
        startNamedTunnel: (
            sessionId: string,
            token: string,
            expectedPublicBaseUrl: string,
        ) => startTunnel(() => options.facade.startNamedTunnel(sessionId, token, expectedPublicBaseUrl)),
        stop: (sessionId: string) => run(async () => {
            try {
                if (snapshot.sourceStatus.tunnel) await options.facade.stopTunnel(sessionId)
                else await options.facade.stop(sessionId)
            } finally {
                const sourceStatus = await refreshSourceState()
                if (['starting', 'running', 'stopping'].includes(sourceStatus.phase)) beginSourcePolling()
                else stopSourcePolling()
            }
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

let androidPeerDeltaController: ReturnType<typeof createPeerDeltaController> | undefined

export function getAndroidPeerDeltaController(runtime: PeerDeltaMutationRuntime) {
    androidPeerDeltaController ??= createPeerDeltaController({
        facade: createPeerDeltaFacade({ platform: 'android', runtime }),
    })
    return androidPeerDeltaController
}
