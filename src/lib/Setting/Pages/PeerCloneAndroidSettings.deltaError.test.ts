// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const controllerMocks = vi.hoisted(() => {
    const listeners = new Set<() => void>()
    let deltaError = ''
    const delta = {
        snapshot: () => ({
            capabilities: {
                desktop: false as const,
                sourceReady: true,
                atomicActivationReady: true,
                authenticatedTransportReady: true,
                productionEnabled: true,
                tunnelReady: false,
            },
            sourceStatus: { phase: 'idle' as const, devices: [] },
            tunnelStatus: { phase: 'idle' as const },
            sourcePairingUri: '',
            pullPhase: 'idle' as const,
            pullResult: undefined,
            error: deltaError,
        }),
        subscribe(listener: () => void) {
            listeners.add(listener)
            listener()
            return () => listeners.delete(listener)
        },
        initialize: vi.fn(async () => undefined),
        prepare: vi.fn(async () => undefined),
        start: vi.fn(async () => undefined),
        stop: vi.fn(async () => undefined),
        pull: vi.fn(async () => undefined),
    }
    const cloneFacade = {
        getState: () => ({
            phase: 'idle' as const,
            destructiveConfirmed: false,
            activationCommitted: false,
            completedBytes: 0,
        }),
        capabilities: vi.fn(async () => ({
            androidClient: true,
            atomicActivationReady: true,
            losslessBackupReady: true,
            httpTransportReady: true,
            productionEnabled: true,
        })),
        recover: vi.fn(async () => null),
        join: vi.fn(),
        confirmDestructiveReplace: vi.fn(),
        download: vi.fn(async () => undefined),
        resume: vi.fn(async () => undefined),
        cancel: vi.fn(async () => undefined),
        targetStatus: vi.fn(async () => undefined),
    }
    const sourceController = {
        snapshot: () => ({ status: { phase: 'idle' as const, devices: [] }, busy: false }),
        refresh: vi.fn(async () => ({ phase: 'idle' as const, devices: [] })),
        prepare: vi.fn(async () => ({ phase: 'idle' as const, devices: [] })),
        start: vi.fn(async () => ({ phase: 'idle' as const, devices: [] })),
        stop: vi.fn(async () => ({ phase: 'idle' as const, devices: [] })),
        revoke: vi.fn(async () => ({ phase: 'idle' as const, devices: [] })),
    }
    return {
        delta,
        cloneFacade,
        sourceController,
        setDeltaError(value: string) {
            deltaError = value
            for (const listener of listeners) listener()
        },
        reset() {
            deltaError = ''
        },
    }
})

vi.mock('src/ts/storage/sync/peerCloneAndroid', () => ({
    getAndroidPeerCloneFacade: () => controllerMocks.cloneFacade,
}))
vi.mock('src/ts/storage/sync/peerCloneAndroidSource', () => ({
    createAndroidPeerCloneSourceFacade: () => ({
        capabilities: vi.fn(async () => ({
            desktop: false,
            sourceReady: true,
            atomicActivationReady: false,
            losslessBackupReady: true,
            httpTransportReady: true,
            largeFixturePassed: true,
            productionEnabled: true,
            tunnelReady: false,
        })),
    }),
}))
vi.mock('src/ts/storage/sync/peerCloneAndroidSourceController', () => ({
    createAndroidPeerCloneSourceController: () => controllerMocks.sourceController,
}))
vi.mock('src/ts/storage/sync/peerDelta', () => ({
    parsePeerDeltaUri: vi.fn(),
}))
vi.mock('src/ts/storage/sync/peerDeltaController', () => ({
    getAndroidPeerDeltaController: () => controllerMocks.delta,
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireDestructiveReplacementFence: vi.fn(),
    capturePersistentMutationToken: vi.fn(),
    flushPendingData: vi.fn(),
}))
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: vi.fn(),
}))
vi.mock('src/ts/storage/sync/peerCloneDeepLink', () => ({
    consumePendingPeerCloneUri: () => '',
    subscribePeerCloneUri: () => () => undefined,
}))
vi.mock('src/ts/alert', () => ({
    alertConfirm: vi.fn(async () => true),
}))
vi.mock('src/lang', async () => ({
    language: (await import('src/lang/en')).languageEnglish,
}))

import PeerCloneAndroidSettings from './PeerCloneAndroidSettings.svelte'

describe('Android peer settings lane errors', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        controllerMocks.reset()
        vi.clearAllMocks()
    })

    it('renders delta-lane errors in the delta section and clears them with the controller', async () => {
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(PeerCloneAndroidSettings, { target })
        await tick()

        controllerMocks.setDeltaError('delta pull failed')
        await tick()
        const [cloneSection, deltaSection] = [...target.querySelectorAll('section')]
        expect(deltaSection?.textContent).toContain('delta pull failed')
        expect(cloneSection?.textContent).not.toContain('delta pull failed')

        controllerMocks.setDeltaError('')
        await tick()
        expect(target.textContent).not.toContain('delta pull failed')
    })
})
