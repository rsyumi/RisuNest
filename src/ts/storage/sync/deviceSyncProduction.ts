import { isTauriAndroid } from '../../platform'
import {
    acquireCommittedWorkingSetRefreshFence,
    acquireDestructiveReplacementFence,
    capturePersistentMutationToken,
    flushPendingData,
} from '../persistentDataRuntime.svelte'
import { createDeviceSyncFacade, DeviceSyncError } from './deviceSync'
import { createAndroidDeviceSyncFacade } from './deviceSyncAndroid'
import {
    createDeviceSyncController,
    type DeviceSyncBidirectionalTarget,
    type DeviceSyncCloneTarget,
    type DeviceSyncController,
    type DeviceSyncDeltaTarget,
} from './deviceSyncController'
import { getDesktopPeerCloneController } from './peerCloneController'
import {
    createAndroidPeerCloneFacade,
    getAndroidPeerCloneFacade,
    type AndroidPeerCloneState,
} from './peerCloneAndroid'
import { getAndroidPeerDeltaController, getDesktopPeerDeltaController } from './peerDeltaController'
import { getDesktopPeerBidirectionalController } from './peerBidirectionalController'
import type { PeerCloneControllerSnapshot } from './peerCloneController'

type AndroidCloneFacade = Pick<
    ReturnType<typeof createAndroidPeerCloneFacade>,
    | 'getState'
    | 'capabilities'
    | 'recover'
    | 'joinRegistered'
    | 'confirmDestructiveReplace'
    | 'download'
    | 'resume'
    | 'cancel'
    | 'targetStatus'
>

type AndroidSchedule = {
    schedule(listener: () => void): unknown
    cancelSchedule(handle: unknown): void
}

function androidTargetPhase(state: AndroidPeerCloneState): PeerCloneControllerSnapshot['state']['target']['phase'] {
    if (state.phase === 'paused') return 'joined'
    if (state.phase === 'idle' || state.phase === 'joined' || state.phase === 'confirmed'
        || state.phase === 'downloading' || state.phase === 'cancelled'
        || state.phase === 'completed' || state.phase === 'failed') return state.phase
    return 'failed'
}

export function createAndroidDeviceSyncCloneTarget(
    facade: AndroidCloneFacade,
    timing: AndroidSchedule = {
        schedule: (listener) => setInterval(listener, 500),
        cancelSchedule: (handle) => clearInterval(handle as ReturnType<typeof setInterval>),
    },
): DeviceSyncCloneTarget & { dispose(): void } {
    const listeners = new Set<(snapshot: PeerCloneControllerSnapshot) => void>()
    let capabilities: Awaited<ReturnType<AndroidCloneFacade['capabilities']>> | undefined
    let timer: unknown
    let polling = false
    let targetEpoch = 0
    let error = ''

    const snapshot = (): PeerCloneControllerSnapshot & {
        platform: 'android'
        resumeAvailable: boolean
    } => {
        const state = facade.getState()
        return {
            platform: 'android',
            resumeAvailable: state.phase === 'paused',
            targetPhase: state.phase === 'downloading'
                ? 'downloading'
                : state.phase === 'cancelled' || state.phase === 'completed' || state.phase === 'failed'
                    ? state.phase
                    : 'idle',
            state: {
                target: {
                    phase: androidTargetPhase(state),
                    destructiveConfirmed: state.destructiveConfirmed,
                    completedBytes: state.completedBytes,
                    ...(state.totalBytes === undefined ? {} : { totalBytes: state.totalBytes }),
                    ...(state.backupPaths === undefined ? {} : { backupPaths: state.backupPaths }),
                },
            },
            error: error || state.error || '',
            warning: capabilities?.productionEnabled === false ? 'unavailable' : '',
        }
    }
    const publish = (): void => {
        const value = snapshot()
        for (const listener of listeners) listener(value)
    }
    const stopPolling = (): void => {
        if (timer !== undefined) timing.cancelSchedule(timer)
        timer = undefined
    }
    const poll = async (): Promise<void> => {
        if (polling) return
        polling = true
        const epoch = targetEpoch
        try {
            const status = await facade.targetStatus()
            if (epoch !== targetEpoch) return
            error = ''
            publish()
            if (status.phase === 'completed' || status.phase === 'cancelled' || status.phase === 'failed') stopPolling()
        } catch (cause) {
            if (epoch !== targetEpoch) return
            error = cause instanceof Error ? cause.message : String(cause)
            publish()
        } finally {
            polling = false
        }
    }
    const startPolling = (): void => {
        if (timer === undefined) timer = timing.schedule(() => { void poll() })
    }
    const run = async <T>(operation: () => Promise<T>): Promise<T> => {
        try {
            const result = await operation()
            error = ''
            publish()
            return result
        } catch (cause) {
            error = cause instanceof Error ? cause.message : String(cause)
            publish()
            throw cause
        }
    }

    return {
        snapshot,
        subscribe(listener): () => void {
            listeners.add(listener)
            listener(snapshot())
            return () => listeners.delete(listener)
        },
        async initialize(): Promise<void> {
            await run(async () => {
                capabilities = await facade.capabilities()
                const recovered = await facade.recover()
                if (recovered && !['completed', 'cancelled', 'failed'].includes(recovered.phase)) startPolling()
            })
        },
        joinClaimed(): void {
            throw new Error('Android registered clone target requires a source device identity')
        },
        joinRegistered(deviceId): Promise<void> {
            return run(async () => {
                await facade.joinRegistered(deviceId)
                targetEpoch += 1
            })
        },
        confirmDestructiveReplace(): void {
            try {
                facade.confirmDestructiveReplace()
                error = ''
                publish()
            } catch (cause) {
                error = cause instanceof Error ? cause.message : String(cause)
                publish()
                throw cause
            }
        },
        download(): Promise<void> {
            return run(async () => {
                await facade.download()
                targetEpoch += 1
                startPolling()
            })
        },
        resume(): Promise<void> {
            return run(async () => {
                await facade.resume()
                targetEpoch += 1
                startPolling()
            })
        },
        cancel(): Promise<void> {
            return run(async () => {
                await facade.cancel()
                targetEpoch += 1
                stopPolling()
            })
        },
        dispose(): void {
            targetEpoch += 1
            stopPolling()
            listeners.clear()
        },
    }
}

async function withRestoredPlugins(
    pendingFence: ReturnType<typeof acquireDestructiveReplacementFence>,
): ReturnType<typeof acquireDestructiveReplacementFence> {
    const fence = await pendingFence
    return {
        ...fence,
        async refreshCommittedWorkingSet(...args) {
            await fence.refreshCommittedWorkingSet(...args)
            const plugins = await import('../../plugins/plugins.svelte')
            await plugins.loadPluginsAfterAuthoritativeRestore()
        },
    }
}

const productionRuntime = {
    flushPendingData,
    capturePersistentMutationToken,
    acquireDestructiveReplacementFence: (
        ...args: Parameters<typeof acquireDestructiveReplacementFence>
    ) => withRestoredPlugins(acquireDestructiveReplacementFence(...args)),
    acquireCommittedWorkingSetRefreshFence: () =>
        withRestoredPlugins(acquireCommittedWorkingSetRefreshFence()),
}

type ProductionFactories = {
    sourceDesktop?(runtime: typeof productionRuntime): ReturnType<typeof createDeviceSyncFacade>
    sourceAndroid?(runtime: typeof productionRuntime): ReturnType<typeof createAndroidDeviceSyncFacade>
    cloneDesktop(runtime: typeof productionRuntime): DeviceSyncCloneTarget
    cloneAndroid(runtime: typeof productionRuntime): DeviceSyncCloneTarget
    deltaDesktop(runtime: typeof productionRuntime): DeviceSyncDeltaTarget
    deltaAndroid(runtime: typeof productionRuntime): DeviceSyncDeltaTarget
    bidirectional(runtime: typeof productionRuntime): DeviceSyncBidirectionalTarget
    controller: typeof createDeviceSyncController
}

function failClosedTarget<T extends { initialize(): Promise<void>; snapshot(): unknown }>(
    target: T,
    hasInitializationError: (snapshot: ReturnType<T['snapshot']>) => boolean,
): T {
    return {
        ...target,
        initialize: async () => {
            try {
                await target.initialize()
            } catch {
                throw new DeviceSyncError('state-unavailable')
            }
            if (hasInitializationError(target.snapshot() as ReturnType<T['snapshot']>)) {
                throw new DeviceSyncError('state-unavailable')
            }
        },
    }
}

const defaultFactories: ProductionFactories = {
    sourceDesktop: (runtime) => createDeviceSyncFacade({ runtime }),
    sourceAndroid: (runtime) => createAndroidDeviceSyncFacade({ runtime }),
    cloneDesktop: (runtime) => getDesktopPeerCloneController(runtime),
    cloneAndroid: (runtime) => createAndroidDeviceSyncCloneTarget(getAndroidPeerCloneFacade({
        capturePersistentMutationToken: runtime.capturePersistentMutationToken,
        acquireDestructiveReplacementFence: runtime.acquireDestructiveReplacementFence,
    })),
    deltaDesktop: (runtime) => getDesktopPeerDeltaController(runtime),
    deltaAndroid: (runtime) => getAndroidPeerDeltaController(runtime),
    bidirectional: (runtime) => getDesktopPeerBidirectionalController(runtime),
    controller: createDeviceSyncController,
}

export function createProductionDeviceSyncController(options: {
    platform?: 'desktop' | 'android'
    runtime?: typeof productionRuntime
    facade?: ReturnType<typeof createDeviceSyncFacade>
    factories?: ProductionFactories
} = {}): DeviceSyncController {
    const platform = options.platform ?? (isTauriAndroid ? 'android' : 'desktop')
    const runtime = options.runtime ?? productionRuntime
    const factories = options.factories ?? defaultFactories
    const rawClone = platform === 'android'
        ? factories.cloneAndroid(runtime)
        : factories.cloneDesktop(runtime)
    const rawDelta = platform === 'android'
        ? factories.deltaAndroid(runtime)
        : factories.deltaDesktop(runtime)
    const rawBidirectional = factories.bidirectional(runtime)
    const clone = failClosedTarget(rawClone, (snapshot) => Boolean(snapshot.error))
    const delta = failClosedTarget(rawDelta, (snapshot) => Boolean(snapshot.error))
    const bidirectional = failClosedTarget(
        rawBidirectional,
        (snapshot) => Boolean(snapshot.operationError),
    )
    return factories.controller({
        facade: options.facade ?? (platform === 'android'
            ? factories.sourceAndroid?.(runtime) ?? createAndroidDeviceSyncFacade({ runtime })
            : factories.sourceDesktop?.(runtime) ?? createDeviceSyncFacade({ runtime })),
        targets: { clone, delta, bidirectional },
    })
}

let productionController: DeviceSyncController | undefined

export function getProductionDeviceSyncController(): DeviceSyncController {
    productionController ??= createProductionDeviceSyncController()
    return productionController
}
