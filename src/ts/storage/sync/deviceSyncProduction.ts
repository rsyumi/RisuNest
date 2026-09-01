import { isTauriAndroid } from '../../platform'
import {
    acquireDestructiveReplacementFence,
    capturePersistentMutationToken,
    flushPendingData,
} from '../persistentDataRuntime.svelte'
import { createDeviceSyncFacade } from './deviceSync'
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
            sourceStatus: { phase: 'idle', devices: [] },
            tunnelStatus: { phase: 'idle' },
            state: {
                source: { phase: 'idle', revokedDeviceIds: [] },
                target: {
                    phase: androidTargetPhase(state),
                    destructiveConfirmed: state.destructiveConfirmed,
                    completedBytes: state.completedBytes,
                    ...(state.totalBytes === undefined ? {} : { totalBytes: state.totalBytes }),
                },
            },
            sourcePairingUri: '',
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
        try {
            const status = await facade.targetStatus()
            error = ''
            publish()
            if (status.phase === 'completed' || status.phase === 'cancelled' || status.phase === 'failed') stopPolling()
        } catch (cause) {
            error = cause instanceof Error ? cause.message : String(cause)
            publish()
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
            return run(async () => { await facade.joinRegistered(deviceId) })
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
            return run(async () => { await facade.download(); startPolling() })
        },
        resume(): Promise<void> {
            return run(async () => { await facade.resume(); startPolling() })
        },
        cancel(): Promise<void> {
            return run(async () => { await facade.cancel(); stopPolling() })
        },
        dispose(): void {
            stopPolling()
            listeners.clear()
        },
    }
}

const productionRuntime = {
    flushPendingData,
    capturePersistentMutationToken,
    acquireDestructiveReplacementFence,
}

type ProductionFactories = {
    sourceDesktop?(): ReturnType<typeof createDeviceSyncFacade>
    sourceAndroid?(runtime: typeof productionRuntime): ReturnType<typeof createAndroidDeviceSyncFacade>
    cloneDesktop(runtime: typeof productionRuntime): DeviceSyncCloneTarget
    cloneAndroid(runtime: typeof productionRuntime): DeviceSyncCloneTarget
    deltaDesktop(runtime: typeof productionRuntime): DeviceSyncDeltaTarget
    deltaAndroid(runtime: typeof productionRuntime): DeviceSyncDeltaTarget
    bidirectional(runtime: typeof productionRuntime): DeviceSyncBidirectionalTarget
    controller: typeof createDeviceSyncController
}

function targetOnly<T extends { initializeTarget(): Promise<void> }>(controller: T): T {
    return { ...controller, initialize: () => controller.initializeTarget() }
}

const defaultFactories: ProductionFactories = {
    sourceDesktop: () => createDeviceSyncFacade(),
    sourceAndroid: (runtime) => createAndroidDeviceSyncFacade({
        flushPendingData: runtime.flushPendingData,
    }),
    cloneDesktop: (runtime) => targetOnly(getDesktopPeerCloneController(runtime)),
    cloneAndroid: (runtime) => createAndroidDeviceSyncCloneTarget(getAndroidPeerCloneFacade({
        capturePersistentMutationToken: runtime.capturePersistentMutationToken,
        acquireDestructiveReplacementFence: runtime.acquireDestructiveReplacementFence,
        afterRefresh: async () => {
            const plugins = await import('../../plugins/plugins.svelte')
            await plugins.loadPluginsAfterAuthoritativeRestore()
        },
    })),
    deltaDesktop: (runtime) => targetOnly(getDesktopPeerDeltaController(runtime)),
    deltaAndroid: (runtime) => targetOnly(getAndroidPeerDeltaController(runtime)),
    bidirectional: (runtime) => targetOnly(getDesktopPeerBidirectionalController(runtime)),
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
    const clone = platform === 'android'
        ? factories.cloneAndroid(runtime)
        : factories.cloneDesktop(runtime)
    const delta = platform === 'android'
        ? factories.deltaAndroid(runtime)
        : factories.deltaDesktop(runtime)
    const bidirectional = factories.bidirectional(runtime)
    return factories.controller({
        facade: options.facade ?? (platform === 'android'
            ? factories.sourceAndroid?.(runtime) ?? createAndroidDeviceSyncFacade({
                flushPendingData: runtime.flushPendingData,
            })
            : factories.sourceDesktop?.() ?? createDeviceSyncFacade()),
        targets: { clone, delta, bidirectional },
    })
}

let productionController: DeviceSyncController | undefined

export function getProductionDeviceSyncController(): DeviceSyncController {
    productionController ??= createProductionDeviceSyncController()
    return productionController
}
