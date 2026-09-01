import type {
    DeviceSyncSettingsInput,
    DeviceSyncLinkPermissions,
    DeviceSyncStatus,
    RegisteredDevice,
    StagedDeviceSyncLink,
    DeviceSyncErrorCode,
} from './deviceSync'
import { createDeviceSyncFacade, DeviceSyncError, parseDeviceSyncUri } from './deviceSync'
import type { RisuNestDeviceSettings } from '../deviceSettings'
import { createPeerSourcePolling } from './peerSourcePolling'

type SourceFacade = {
    status(): Promise<DeviceSyncStatus>
    prepare(settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus>
    start(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus>
    stop(): Promise<void>
    rotateLink(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus>
    incomingSources(): Promise<RegisteredDevice[]>
    outgoingDevices(): Promise<RegisteredDevice[]>
    revokeIncoming(deviceId: string): Promise<void>
    revokeOutgoing(deviceId: string): Promise<void>
    claimStagedClone?(link: StagedDeviceSyncLink): Promise<{ sourceDeviceId: string, endpoint: string, sessionId: string, manifestId: string }>
}

type TargetController = {
    snapshot(): unknown
    subscribe(listener: (snapshot: unknown) => void): () => void
}

export interface DeviceSyncControllerSnapshot {
    source: DeviceSyncStatus
    sources: RegisteredDevice[]
    devices: RegisteredDevice[]
    error: DeviceSyncErrorCode | null
    stagedLink: StagedDeviceSyncLink | null
    expiredSourceIds: readonly string[]
    targets: { clone?: unknown, delta?: unknown, bidirectional?: unknown }
}

const initialSnapshot: DeviceSyncControllerSnapshot = {
    source: { phase: 'idle' },
    sources: [],
    devices: [],
    error: null,
    stagedLink: null,
    expiredSourceIds: [],
    targets: {},
}

function safeError(error: unknown): DeviceSyncErrorCode {
    return error instanceof DeviceSyncError ? error.code : 'unavailable'
}

function sourceIsOff(status: DeviceSyncStatus): boolean {
    return status.phase === 'idle'
}

export function createDeviceSyncController(options: {
    facade: SourceFacade
    sourcePollMilliseconds?: number
    targets?: {
        clone?: TargetController & { joinClaimed(target: { endpoint: string, sessionId: string, manifestId: string }): void }
        delta?: TargetController & { pullRegistered(deviceId: string): Promise<unknown> }
        bidirectional?: TargetController & {
            syncRegistered(deviceId: string): Promise<unknown>
            resolveRegistered(deviceId: string, winner: 'local' | 'remote'): Promise<unknown>
        }
    }
}) {
    const listeners = new Set<(snapshot: DeviceSyncControllerSnapshot) => void>()
    let snapshot = initialSnapshot
    let initialized = false
    let initialization: Promise<void> | undefined
    let sourceWork: Promise<unknown> | undefined
    let targetUnsubscribers: Array<(() => void) | undefined> = []

    const publish = (): void => {
        snapshot = { ...snapshot }
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<DeviceSyncControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        publish()
    }
    targetUnsubscribers = [
        options.targets?.clone && options.targets.clone.subscribe((clone) => update({ targets: { ...snapshot.targets, clone } })),
        options.targets?.delta && options.targets.delta.subscribe((delta) => update({ targets: { ...snapshot.targets, delta } })),
        options.targets?.bidirectional && options.targets.bidirectional.subscribe((bidirectional) => update({ targets: { ...snapshot.targets, bidirectional } })),
    ]
    const refreshRegistries = async (): Promise<void> => {
        const [sources, devices] = await Promise.all([
            options.facade.incomingSources(),
            options.facade.outgoingDevices(),
        ])
        update({ sources, devices })
    }
    const polling = createPeerSourcePolling({
        intervalMilliseconds: options.sourcePollMilliseconds ?? 1_000,
        poll: async () => {
            try {
                const source = await options.facade.status()
                update({ source, error: source.error ?? null })
                if (!['prepared', 'starting', 'running', 'stopping'].includes(source.phase)) polling.stop()
            } catch (error) {
                update({ error: safeError(error) })
                polling.stop()
            }
        },
    })
    const observeSource = (source: DeviceSyncStatus): void => {
        if (['prepared', 'starting', 'running', 'stopping'].includes(source.phase)) polling.start()
        else polling.stop()
    }
    const runSource = <T>(operation: () => Promise<T>): Promise<T> => {
        if (sourceWork) return Promise.reject(new Error('Another sharing action is already running'))
        const work = operation().finally(() => { sourceWork = undefined })
        sourceWork = work
        return work
    }
    const updateSource = async (operation: () => Promise<DeviceSyncStatus>): Promise<DeviceSyncStatus> => {
        try {
            const source = await operation()
            update({ source, error: source.error ?? null })
            observeSource(source)
            return source
        } catch (error) {
            update({ error: safeError(error) })
            throw new Error(snapshot.error)
        }
    }
    const completeReceive = async <T>(operation: () => Promise<T>): Promise<T> => {
        if (!sourceIsOff(snapshot.source)) throw new Error('Sharing is active. Stop sharing before receiving.')
        try {
            const result = await operation()
            await refreshRegistries()
            return result
        } catch (error) {
            update({ error: safeError(error) })
            throw new Error(snapshot.error ?? 'unavailable')
        }
    }
    const requireSource = (deviceId: string, permission: 'read' | 'bidirectional'): void => {
        const source = snapshot.sources.find((candidate) => candidate.deviceId === deviceId)
        if (!source || !source.permissions.includes(permission)) throw new Error('unavailable')
    }
    const registeredReceive = async <T>(deviceId: string, permission: 'read' | 'bidirectional', operation: () => Promise<T>): Promise<T> => {
        requireSource(deviceId, permission)
        try {
            return await completeReceive(operation)
        } catch (error) {
            if (error instanceof Error && error.message === 'registration-expired') {
                update({ expiredSourceIds: [...new Set([...snapshot.expiredSourceIds, deviceId])] })
            }
            throw error
        }
    }
    const claimStaged = async (): Promise<{ sourceDeviceId: string, endpoint: string, sessionId: string, manifestId: string }> => {
        if (!snapshot.stagedLink || !options.facade.claimStagedClone) throw new Error('unavailable')
        return completeReceive(() => options.facade.claimStagedClone!(snapshot.stagedLink!))
    }

    return {
        snapshot: (): DeviceSyncControllerSnapshot => snapshot,
        subscribe(listener: (value: DeviceSyncControllerSnapshot) => void): () => void {
            listeners.add(listener)
            listener(snapshot)
            return () => listeners.delete(listener)
        },
        initialize(): Promise<void> {
            if (initialized) return initialization ?? Promise.resolve()
            initialized = true
            initialization = Promise.all([options.facade.status(), refreshRegistries()]).then(([source]) => {
                update({ source, error: source.error ?? null })
                observeSource(source)
            }).catch((error) => {
                initialized = false
                initialization = undefined
                update({ error: safeError(error) })
            })
            return initialization
        },
        prepare(settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus> {
            return runSource(() => updateSource(() => options.facade.prepare(settings)))
        },
        start(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> {
            return runSource(() => updateSource(() => options.facade.start(permissions)))
        },
        stop(): Promise<DeviceSyncStatus> {
            return runSource(async () => {
                await options.facade.stop()
                return updateSource(() => options.facade.status())
            })
        },
        rotateLink(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> {
            return runSource(() => updateSource(() => options.facade.rotateLink(permissions)))
        },
        async revokeIncoming(deviceId: string): Promise<void> {
            await options.facade.revokeIncoming(deviceId)
            await refreshRegistries()
        },
        async revokeOutgoing(deviceId: string): Promise<void> {
            await options.facade.revokeOutgoing(deviceId)
            await refreshRegistries()
        },
        async completeReceive<T>(operation: () => Promise<T>): Promise<T> {
            return completeReceive(operation)
        },
        stageLink(uri: string): void {
            update({ stagedLink: parseDeviceSyncUri(uri), error: null })
        },
        clearStagedLink(): void {
            update({ stagedLink: null })
        },
        async claimStagedClone(): Promise<void> {
            if (!options.targets?.clone) throw new Error('unavailable')
            await completeReceive(async () => {
                const target = await claimStaged()
                options.targets!.clone!.joinClaimed(target)
            })
            update({ stagedLink: null })
        },
        async pullStagedDelta(): Promise<unknown> {
            if (!options.targets?.delta) throw new Error('unavailable')
            const target = await claimStaged()
            const result = await registeredReceive(target.sourceDeviceId, 'read', () => options.targets!.delta!.pullRegistered(target.sourceDeviceId))
            update({ stagedLink: null })
            return result
        },
        async syncStagedBidirectional(): Promise<unknown> {
            if (!options.targets?.bidirectional) throw new Error('unavailable')
            const target = await claimStaged()
            const result = await registeredReceive(target.sourceDeviceId, 'bidirectional', () => options.targets!.bidirectional!.syncRegistered(target.sourceDeviceId))
            update({ stagedLink: null })
            return result
        },
        pullRegisteredDelta(deviceId: string): Promise<unknown> {
            if (!options.targets?.delta) return Promise.reject(new Error('Delta target is unavailable'))
            return registeredReceive(deviceId, 'read', () => options.targets!.delta!.pullRegistered(deviceId))
        },
        syncRegisteredBidirectional(deviceId: string): Promise<unknown> {
            if (!options.targets?.bidirectional) return Promise.reject(new Error('Bidirectional target is unavailable'))
            return registeredReceive(deviceId, 'bidirectional', () => options.targets!.bidirectional!.syncRegistered(deviceId))
        },
        resolveRegisteredBidirectional(deviceId: string, winner: 'local' | 'remote'): Promise<unknown> {
            if (!options.targets?.bidirectional) return Promise.reject(new Error('Bidirectional target is unavailable'))
            return registeredReceive(deviceId, 'bidirectional', () => options.targets!.bidirectional!.resolveRegistered(deviceId, winner))
        },
        dispose(): void {
            polling.stop()
            for (const unsubscribe of targetUnsubscribers) unsubscribe?.()
        },
    }
}

let singleton: ReturnType<typeof createDeviceSyncController> | undefined

export function getDeviceSyncController(options: Parameters<typeof createDeviceSyncController>[0]) {
    singleton ??= createDeviceSyncController(options)
    return singleton
}

export async function startDeviceSyncAutoListen(
    settings: Pick<RisuNestDeviceSettings, 'syncAutoListen' | 'syncListenMethod' | 'syncFixedPort' | 'syncPublicBaseUrl'>,
    options: {
        controller?: Pick<ReturnType<typeof createDeviceSyncController>, 'initialize' | 'prepare' | 'start'>
        report?: (error: unknown) => void
    } = {},
): Promise<void> {
    if (!settings.syncAutoListen) return
    const controller = options.controller ?? getDeviceSyncController({ facade: createDeviceSyncFacade() })
    try {
        await controller.initialize()
        await controller.prepare({
            method: settings.syncListenMethod,
            fixedPort: settings.syncFixedPort,
            publicBaseUrl: settings.syncPublicBaseUrl,
        })
        await controller.start({ read: true, bidirectional: false })
    } catch (error) {
        options.report?.(error)
    }
}
