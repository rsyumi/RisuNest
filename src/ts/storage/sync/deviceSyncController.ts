import type {
    DeviceSyncSettingsInput,
    DeviceSyncStatus,
    RegisteredDevice,
} from './deviceSync'
import { createDeviceSyncFacade } from './deviceSync'
import type { RisuNestDeviceSettings } from '../deviceSettings'
import { createPeerSourcePolling } from './peerSourcePolling'

type SourceFacade = {
    status(): Promise<DeviceSyncStatus>
    prepare(settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus>
    start(settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus>
    stop(): Promise<DeviceSyncStatus>
    rotateLink(): Promise<DeviceSyncStatus>
    incomingSources(): Promise<RegisteredDevice[]>
    outgoingDevices(): Promise<RegisteredDevice[]>
    revokeIncoming(deviceId: string): Promise<void>
    revokeOutgoing(deviceId: string): Promise<void>
}

export interface DeviceSyncControllerSnapshot {
    source: DeviceSyncStatus
    sources: RegisteredDevice[]
    devices: RegisteredDevice[]
    error: string
}

const initialSnapshot: DeviceSyncControllerSnapshot = {
    source: { phase: 'idle' },
    sources: [],
    devices: [],
    error: '',
}

function safeError(error: unknown): string {
    const message = error instanceof Error ? error.message : String(error)
    if (message.includes('port')) return 'The selected port is unavailable.'
    if (message.includes('401') || message.includes('registration')) {
        return 'This registration has expired. Register this device again.'
    }
    return 'Could not update sharing.'
}

function sourceIsOff(status: DeviceSyncStatus): boolean {
    return status.phase === 'idle'
}

export function createDeviceSyncController(options: {
    facade: SourceFacade
    sourcePollMilliseconds?: number
}) {
    const listeners = new Set<(snapshot: DeviceSyncControllerSnapshot) => void>()
    let snapshot = initialSnapshot
    let initialized = false
    let initialization: Promise<void> | undefined
    let sourceWork: Promise<unknown> | undefined

    const publish = (): void => {
        snapshot = { ...snapshot }
        for (const listener of listeners) listener(snapshot)
    }
    const update = (next: Partial<DeviceSyncControllerSnapshot>): void => {
        snapshot = { ...snapshot, ...next }
        publish()
    }
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
                update({ source, error: '' })
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
            update({ source, error: '' })
            observeSource(source)
            return source
        } catch (error) {
            update({ error: safeError(error) })
            throw new Error(snapshot.error)
        }
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
                update({ source, error: '' })
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
        start(settings: DeviceSyncSettingsInput): Promise<DeviceSyncStatus> {
            return runSource(() => updateSource(() => options.facade.start(settings)))
        },
        stop(): Promise<DeviceSyncStatus> {
            return runSource(() => updateSource(() => options.facade.stop()))
        },
        rotateLink(): Promise<DeviceSyncStatus> {
            return runSource(() => updateSource(() => options.facade.rotateLink()))
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
            if (!sourceIsOff(snapshot.source)) throw new Error('Sharing is active. Stop sharing before receiving.')
            try {
                const result = await operation()
                await refreshRegistries()
                return result
            } catch (error) {
                update({ error: safeError(error) })
                throw new Error(snapshot.error)
            }
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
        controller?: Pick<ReturnType<typeof createDeviceSyncController>, 'initialize' | 'start'>
        report?: (error: unknown) => void
    } = {},
): Promise<void> {
    if (!settings.syncAutoListen) return
    const controller = options.controller ?? getDeviceSyncController({ facade: createDeviceSyncFacade() })
    try {
        await controller.initialize()
        await controller.start({
            method: settings.syncListenMethod,
            fixedPort: settings.syncFixedPort,
            publicBaseUrl: settings.syncPublicBaseUrl,
        })
    } catch (error) {
        options.report?.(error)
    }
}
