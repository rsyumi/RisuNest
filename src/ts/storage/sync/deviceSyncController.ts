import type {
    DeviceSyncSettingsInput,
    DeviceSyncLinkPermissions,
    DeviceSyncStatus,
    RegisteredCloneSession,
    RegisteredDevice,
    StagedDeviceSyncLink,
    DeviceSyncErrorCode,
} from './deviceSync'
import { classifyDeviceSyncFailure, DeviceSyncError, parseDeviceSyncUri } from './deviceSync'
import type { RisuNestDeviceSettings } from '../deviceSettings'
import { createPeerSourcePolling } from './peerSourcePolling'
import type { PeerCloneControllerSnapshot } from './peerCloneController'
import type { PeerDeltaControllerSnapshot } from './peerDeltaController'
import type { PeerBidirectionalControllerSnapshot } from './peerBidirectionalController'
import { consumePendingDeviceSyncUri, subscribeDeviceSyncUri } from './peerCloneDeepLink'

export type SafePeerCloneControllerSnapshot = Omit<PeerCloneControllerSnapshot, 'error' | 'warning'> & {
    error: DeviceSyncErrorCode | null
    warning: DeviceSyncErrorCode | null
    platform?: 'desktop' | 'android'
    resumeAvailable?: boolean
}
export type SafePeerDeltaControllerSnapshot = Omit<PeerDeltaControllerSnapshot, 'error'> & {
    error: DeviceSyncErrorCode | null
}
export type SafePeerBidirectionalControllerSnapshot = Omit<
    PeerBidirectionalControllerSnapshot,
    'sourceError' | 'operationError'
> & {
    sourceError: DeviceSyncErrorCode | null
    operationError: DeviceSyncErrorCode | null
}

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
    claimStagedClone?(link: StagedDeviceSyncLink): Promise<RegisteredCloneSession>
    reconnectRegisteredClone?(deviceId: string): Promise<RegisteredCloneSession>
}

export type DeviceSyncCloneTarget = {
    snapshot(): PeerCloneControllerSnapshot & { platform?: 'desktop' | 'android'; resumeAvailable?: boolean }
    subscribe(listener: (snapshot: PeerCloneControllerSnapshot & {
        platform?: 'desktop' | 'android'
        resumeAvailable?: boolean
    }) => void): () => void
    initialize(): Promise<void>
    joinClaimed(target: Omit<RegisteredCloneSession, 'sourceDeviceId'>): void
    joinRegistered?(deviceId: string): Promise<void>
    confirmDestructiveReplace(): void
    download(): Promise<void>
    resume(): Promise<void>
    cancel(): Promise<void>
    dispose?(): void
}
export type DeviceSyncDeltaTarget = {
    snapshot(): PeerDeltaControllerSnapshot
    subscribe(listener: (snapshot: PeerDeltaControllerSnapshot) => void): () => void
    initialize(): Promise<void>
    pullRegistered(deviceId: string): Promise<unknown>
}
export type DeviceSyncBidirectionalTarget = {
    snapshot(): PeerBidirectionalControllerSnapshot
    subscribe(listener: (snapshot: PeerBidirectionalControllerSnapshot) => void): () => void
    initialize(): Promise<void>
    syncRegistered(deviceId: string): Promise<unknown>
    resolveRegistered(deviceId: string, winner: 'local' | 'remote'): Promise<unknown>
    resume(): Promise<unknown>
    acknowledge(): Promise<void>
    abandon(): Promise<void>
}

export interface DeviceSyncControllerSnapshot {
    source: DeviceSyncStatus
    sources: RegisteredDevice[]
    devices: RegisteredDevice[]
    error: DeviceSyncErrorCode | null
    stagedLink: StagedDeviceSyncLink | null
    stagedSourceDeviceId: string | null
    expiredSourceIds: readonly string[]
    targets: {
        clone?: SafePeerCloneControllerSnapshot
        delta?: SafePeerDeltaControllerSnapshot
        bidirectional?: SafePeerBidirectionalControllerSnapshot
    }
}

function createInitialSnapshot(): DeviceSyncControllerSnapshot {
    return {
        source: { phase: 'idle' }, sources: [], devices: [], error: null,
        stagedLink: null, stagedSourceDeviceId: null, expiredSourceIds: [], targets: {},
    }
}

function safeCode(value: unknown): DeviceSyncErrorCode | null {
    if (value === undefined || value === null || value === '') return null
    return classifyDeviceSyncFailure(value).code
}
function safeClone(value: PeerCloneControllerSnapshot): SafePeerCloneControllerSnapshot {
    return { ...value, error: safeCode(value.error), warning: safeCode(value.warning) }
}
function safeDelta(value: PeerDeltaControllerSnapshot): SafePeerDeltaControllerSnapshot {
    return { ...value, error: safeCode(value.error) }
}
function safeBidirectional(value: PeerBidirectionalControllerSnapshot): SafePeerBidirectionalControllerSnapshot {
    return { ...value, sourceError: safeCode(value.sourceError), operationError: safeCode(value.operationError) }
}

export function createDeviceSyncController(options: {
    facade: SourceFacade
    sourcePollMilliseconds?: number
    targets?: {
        clone?: DeviceSyncCloneTarget
        delta?: DeviceSyncDeltaTarget
        bidirectional?: DeviceSyncBidirectionalTarget
    }
    deepLinks?: {
        consumePending(): string | null
        subscribe(listener: (uri: string) => void): () => void
    }
}) {
    const listeners = new Set<(snapshot: DeviceSyncControllerSnapshot) => void>()
    let snapshot = createInitialSnapshot()
    let initialized = false
    let initialization: Promise<void> | undefined
    let activeWork: Promise<unknown> | undefined
    let disposed = false

    const publish = (): void => {
        snapshot = { ...snapshot, targets: { ...snapshot.targets } }
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<DeviceSyncControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        publish()
    }
    const fail = (error: unknown, deviceId?: string): DeviceSyncError => {
        const safe = classifyDeviceSyncFailure(error)
        update({
            error: safe.code,
            expiredSourceIds: safe.code === 'registration-expired' && deviceId
                ? [...new Set([...snapshot.expiredSourceIds, deviceId])]
                : snapshot.expiredSourceIds,
        })
        return safe
    }
    const clearExpired = (deviceId: string): void => update({
        error: null,
        expiredSourceIds: snapshot.expiredSourceIds.filter((candidate) => candidate !== deviceId),
    })
    const runExclusive = <T>(operation: () => Promise<T>): Promise<T> => {
        if (activeWork) return Promise.reject(new DeviceSyncError('unavailable'))
        const work = Promise.resolve().then(operation).finally(() => {
            if (activeWork === work) activeWork = undefined
        })
        activeWork = work
        return work
    }
    const refreshRegistries = async (): Promise<void> => {
        const [sources, devices] = await Promise.all([
            options.facade.incomingSources(), options.facade.outgoingDevices(),
        ])
        update({ sources, devices })
    }
    const polling = createPeerSourcePolling({
        intervalMilliseconds: options.sourcePollMilliseconds ?? 1_000,
        poll: async () => {
            try {
                const source = await options.facade.status()
                update({ source, error: source.latestError ?? null })
                if (!['preparing', 'prepared', 'starting', 'running', 'stopping'].includes(source.phase)) polling.stop()
            } catch (error) {
                fail(error)
                polling.stop()
            }
        },
    })
    const observeSource = (source: DeviceSyncStatus): void => {
        if (['preparing', 'prepared', 'starting', 'running', 'stopping'].includes(source.phase)) polling.start()
        else polling.stop()
    }
    const updateSource = async (operation: () => Promise<DeviceSyncStatus>): Promise<DeviceSyncStatus> => {
        try {
            const source = await operation()
            update({ source, error: source.latestError ?? null })
            observeSource(source)
            return source
        } catch (error) {
            throw fail(error)
        }
    }
    const ensureReceiveAllowed = (): void => {
        if (snapshot.source.phase !== 'idle') throw new DeviceSyncError('unavailable')
    }
    const receive = async <T>(operation: () => Promise<T>, deviceId?: string): Promise<T> => {
        ensureReceiveAllowed()
        try {
            const result = await operation()
            await refreshRegistries()
            update({ error: null })
            return result
        } catch (error) {
            throw fail(error, deviceId)
        }
    }
    const requireSource = (deviceId: string, permission: 'read' | 'bidirectional'): void => {
        const source = snapshot.sources.find((candidate) => candidate.deviceId === deviceId)
        if (!source?.permissions.includes(permission)) throw new DeviceSyncError('unavailable')
    }
    const registeredReceive = <T>(
        deviceId: string,
        permission: 'read' | 'bidirectional',
        operation: () => Promise<T>,
    ): Promise<T> => {
        requireSource(deviceId, permission)
        return receive(operation, deviceId)
    }
    const claimStaged = async (): Promise<string> => {
        if (snapshot.stagedSourceDeviceId) return snapshot.stagedSourceDeviceId
        const link = snapshot.stagedLink
        if (!link || !options.facade.claimStagedClone) throw new DeviceSyncError('unavailable')
        let claimed: RegisteredCloneSession
        try {
            claimed = await options.facade.claimStagedClone(link)
        } catch (error) {
            throw fail(error)
        }
        update({
            stagedLink: null,
            stagedSourceDeviceId: claimed.sourceDeviceId,
            error: null,
            expiredSourceIds: snapshot.expiredSourceIds.filter(
                (candidate) => candidate !== claimed.sourceDeviceId,
            ),
        })
        await refreshRegistries()
        return claimed.sourceDeviceId
    }
    const joinRegisteredClone = async (deviceId: string): Promise<void> => {
        const target = options.targets?.clone
        if (!target) throw new DeviceSyncError('unavailable')
        requireSource(deviceId, 'read')
        if (target.joinRegistered) {
            await target.joinRegistered(deviceId)
            return
        }
        if (!options.facade.reconnectRegisteredClone) throw new DeviceSyncError('unavailable')
        const descriptor = await options.facade.reconnectRegisteredClone(deviceId)
        target.joinClaimed({
            endpoint: descriptor.endpoint,
            sessionId: descriptor.sessionId,
            manifestId: descriptor.manifestId,
        })
    }
    const stageLink = (uri: string): void => {
        try {
            update({ stagedLink: parseDeviceSyncUri(uri), stagedSourceDeviceId: null, error: null })
        } catch (error) {
            fail(error)
        }
    }

    const targetUnsubscribers = [
        options.targets?.clone?.subscribe((value) => update({
            targets: { ...snapshot.targets, clone: safeClone(value) },
        })),
        options.targets?.delta?.subscribe((value) => update({
            targets: { ...snapshot.targets, delta: safeDelta(value) },
        })),
        options.targets?.bidirectional?.subscribe((value) => update({
            targets: { ...snapshot.targets, bidirectional: safeBidirectional(value) },
        })),
    ]
    const deepLinks = options.deepLinks ?? {
        consumePending: consumePendingDeviceSyncUri,
        subscribe: subscribeDeviceSyncUri,
    }
    const pendingLink = deepLinks.consumePending()
    if (pendingLink) stageLink(pendingLink)
    const unsubscribeDeepLink = deepLinks.subscribe(stageLink)

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
            initialization = Promise.all([
                options.targets?.clone?.initialize(),
                options.targets?.delta?.initialize(),
                options.targets?.bidirectional?.initialize(),
                options.facade.status(),
                refreshRegistries(),
            ]).then(([, , , source]) => {
                update({ source, error: source.latestError ?? null })
                observeSource(source)
            }).catch((error) => {
                initialized = false
                initialization = undefined
                throw fail(error)
            })
            return initialization
        },
        prepare(settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus> {
            return runExclusive(() => updateSource(() => options.facade.prepare(settings)))
        },
        start(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> {
            return runExclusive(() => updateSource(() => options.facade.start(permissions)))
        },
        stop(): Promise<DeviceSyncStatus> {
            return runExclusive(async () => {
                try { await options.facade.stop() } catch (error) { throw fail(error) }
                return updateSource(() => options.facade.status())
            })
        },
        rotateLink(permissions: DeviceSyncLinkPermissions): Promise<DeviceSyncStatus> {
            return runExclusive(() => updateSource(() => options.facade.rotateLink(permissions)))
        },
        revokeIncoming(deviceId: string): Promise<void> {
            return runExclusive(async () => {
                try {
                    await options.facade.revokeIncoming(deviceId)
                    await refreshRegistries()
                    clearExpired(deviceId)
                } catch (error) { throw fail(error) }
            })
        },
        revokeOutgoing(deviceId: string): Promise<void> {
            return runExclusive(async () => {
                try {
                    await options.facade.revokeOutgoing(deviceId)
                    await refreshRegistries()
                } catch (error) { throw fail(error) }
            })
        },
        completeReceive<T>(operation: () => Promise<T>): Promise<T> {
            return runExclusive(() => receive(operation))
        },
        stageLink,
        clearStagedLink(): void { update({ stagedLink: null, stagedSourceDeviceId: null }) },
        claimStagedClone(): Promise<void> {
            return runExclusive(async () => {
                ensureReceiveAllowed()
                const deviceId = await claimStaged()
                try {
                    await joinRegisteredClone(deviceId)
                    clearExpired(deviceId)
                } catch (error) { throw fail(error, deviceId) }
            })
        },
        selectRegisteredClone(deviceId: string): Promise<void> {
            return runExclusive(async () => {
                ensureReceiveAllowed()
                try {
                    await joinRegisteredClone(deviceId)
                    clearExpired(deviceId)
                } catch (error) { throw fail(error, deviceId) }
            })
        },
        confirmCloneReplace(): Promise<void> {
            return runExclusive(async () => {
                ensureReceiveAllowed()
                try {
                    if (!options.targets?.clone) throw new DeviceSyncError('unavailable')
                    options.targets.clone.confirmDestructiveReplace()
                } catch (error) { throw fail(error) }
            })
        },
        downloadClone(): Promise<void> {
            return runExclusive(() => receive(async () => {
                if (!options.targets?.clone) throw new DeviceSyncError('unavailable')
                await options.targets.clone.download()
            }, snapshot.stagedSourceDeviceId ?? undefined))
        },
        resumeClone(): Promise<void> {
            return runExclusive(() => receive(async () => {
                if (!options.targets?.clone) throw new DeviceSyncError('unavailable')
                await options.targets.clone.resume()
            }, snapshot.stagedSourceDeviceId ?? undefined))
        },
        cancelClone(): Promise<void> {
            return runExclusive(() => receive(async () => {
                if (!options.targets?.clone) throw new DeviceSyncError('unavailable')
                await options.targets.clone.cancel()
            }, snapshot.stagedSourceDeviceId ?? undefined))
        },
        pullStagedDelta(): Promise<unknown> {
            return runExclusive(async () => {
                ensureReceiveAllowed()
                const deviceId = await claimStaged()
                if (!options.targets?.delta) throw new DeviceSyncError('unavailable')
                return registeredReceive(deviceId, 'read', () => options.targets!.delta!.pullRegistered(deviceId))
            })
        },
        syncStagedBidirectional(): Promise<unknown> {
            return runExclusive(async () => {
                ensureReceiveAllowed()
                const deviceId = await claimStaged()
                if (!options.targets?.bidirectional) throw new DeviceSyncError('unavailable')
                return registeredReceive(deviceId, 'bidirectional', () => (
                    options.targets!.bidirectional!.syncRegistered(deviceId)
                ))
            })
        },
        pullRegisteredDelta(deviceId: string): Promise<unknown> {
            return runExclusive(async () => {
                if (!options.targets?.delta) throw new DeviceSyncError('unavailable')
                return registeredReceive(deviceId, 'read', () => options.targets!.delta!.pullRegistered(deviceId))
            })
        },
        syncRegisteredBidirectional(deviceId: string): Promise<unknown> {
            return runExclusive(async () => {
                if (!options.targets?.bidirectional) throw new DeviceSyncError('unavailable')
                return registeredReceive(deviceId, 'bidirectional', () => (
                    options.targets!.bidirectional!.syncRegistered(deviceId)
                ))
            })
        },
        resolveRegisteredBidirectional(deviceId: string, winner: 'local' | 'remote'): Promise<unknown> {
            return runExclusive(async () => {
                if (!options.targets?.bidirectional) throw new DeviceSyncError('unavailable')
                return registeredReceive(deviceId, 'bidirectional', () => (
                    options.targets!.bidirectional!.resolveRegistered(deviceId, winner)
                ))
            })
        },
        resumeBidirectional(): Promise<unknown> {
            return runExclusive(() => receive(async () => {
                if (!options.targets?.bidirectional) throw new DeviceSyncError('unavailable')
                return options.targets.bidirectional.resume()
            }))
        },
        acknowledgeBidirectional(): Promise<void> {
            return runExclusive(() => receive(async () => {
                if (!options.targets?.bidirectional) throw new DeviceSyncError('unavailable')
                await options.targets.bidirectional.acknowledge()
            }))
        },
        abandonBidirectional(): Promise<void> {
            return runExclusive(() => receive(async () => {
                if (!options.targets?.bidirectional) throw new DeviceSyncError('unavailable')
                await options.targets.bidirectional.abandon()
            }))
        },
        dispose(): void {
            if (disposed) return
            disposed = true
            polling.stop()
            unsubscribeDeepLink()
            for (const unsubscribe of targetUnsubscribers) unsubscribe?.()
            options.targets?.clone?.dispose?.()
            listeners.clear()
        },
    }
}

export type DeviceSyncController = ReturnType<typeof createDeviceSyncController>

export async function startDeviceSyncAutoListen(
    settings: Pick<RisuNestDeviceSettings, 'syncAutoListen' | 'syncListenMethod' | 'syncFixedPort' | 'syncPublicBaseUrl'>,
    options: {
        controller: Pick<DeviceSyncController, 'initialize' | 'prepare' | 'start'>
        report?: (error: unknown) => void
    },
): Promise<void> {
    if (!settings.syncAutoListen) return
    try {
        await options.controller.initialize()
        await options.controller.prepare({
            method: settings.syncListenMethod,
            fixedPort: settings.syncFixedPort,
            publicBaseUrl: settings.syncPublicBaseUrl,
        })
        await options.controller.start({ read: true, bidirectional: false })
    } catch (error) {
        options.report?.(classifyDeviceSyncFailure(error))
    }
}
