// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const controllerMocks = vi.hoisted(() => {
    const listeners = new Set<() => void>()
    let pullPhase: 'idle' | 'running' = 'running'
    const delta = {
        snapshot: () => ({
            capabilities: {
                desktop: true as const,
                sourceReady: true,
                atomicActivationReady: true,
                authenticatedTransportReady: true,
                productionEnabled: true,
                tunnelReady: true,
            },
            sourceStatus: { phase: 'prepared' as const, sessionId: 'session', devices: [] },
            tunnelStatus: { phase: 'idle' as const },
            sourcePairingUri: '',
            pullPhase,
            pullResult: undefined,
            error: '',
        }),
        subscribe(listener: () => void) {
            listeners.add(listener)
            listener()
            return () => listeners.delete(listener)
        },
        initialize: vi.fn(async () => undefined),
        prepare: vi.fn(async () => undefined),
        pull: vi.fn(async () => undefined),
    }
    const clone = {
        snapshot: () => ({ sourcePairingUri: '' }),
        subscribe: () => () => undefined,
        initialize: vi.fn(async () => undefined),
    }
    const bidirectional = {
        subscribe: () => () => undefined,
        initialize: vi.fn(async () => undefined),
    }
    return {
        delta,
        clone,
        bidirectional,
        setPullPhase(value: 'idle' | 'running') {
            pullPhase = value
            for (const listener of listeners) listener()
        },
        reset() {
            pullPhase = 'running'
        },
    }
})

vi.mock('src/ts/storage/sync/peerClone', () => ({
    initialPeerCloneState: {
        source: { phase: 'idle', devices: [] },
        target: { phase: 'idle', completedBytes: 0 },
    },
    pairingUriForQr: (value: string) => value,
}))
vi.mock('src/ts/storage/sync/peerDelta', () => ({
    parsePeerDeltaUri: vi.fn(),
}))
vi.mock('src/ts/storage/sync/peerBidirectional', () => ({
    parsePeerBidirectionalUri: vi.fn(),
}))
vi.mock('src/ts/storage/sync/peerCloneController', () => ({
    getDesktopPeerCloneController: () => controllerMocks.clone,
}))
vi.mock('src/ts/storage/sync/peerDeltaController', () => ({
    getDesktopPeerDeltaController: () => controllerMocks.delta,
}))
vi.mock('src/ts/storage/sync/peerBidirectionalController', () => ({
    getDesktopPeerBidirectionalController: () => controllerMocks.bidirectional,
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireDestructiveReplacementFence: vi.fn(),
    capturePersistentMutationToken: vi.fn(),
    flushPendingData: vi.fn(),
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

import PeerCloneSettings from './PeerCloneSettings.svelte'

describe('peer delta settings gating', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        controllerMocks.reset()
        vi.clearAllMocks()
    })

    it('keeps delta actions disabled across remount while the controller reports a running pull', async () => {
        const target = document.createElement('div')
        document.body.append(target)
        const deltaSection = () => [...target.querySelectorAll('section')]
            .find((section) => section.querySelector('h3')?.textContent === 'Incremental update')
        const button = (label: string) => [...deltaSection()?.querySelectorAll<HTMLButtonElement>('button') ?? []]
            .find((candidate) => candidate.textContent?.trim() === label)

        mounted = mount(PeerCloneSettings, { target })
        await tick()
        expect(button('Prepare update')?.disabled).toBe(true)
        expect(button('Start sharing')?.disabled).toBe(true)
        await unmount(mounted)
        mounted = undefined
        target.replaceChildren()

        mounted = mount(PeerCloneSettings, { target })
        await tick()
        expect(button('Prepare update')?.disabled).toBe(true)
        expect(button('Start sharing')?.disabled).toBe(true)

        controllerMocks.setPullPhase('idle')
        await tick()
        expect(button('Prepare update')?.disabled).toBe(false)
        expect(button('Start sharing')?.disabled).toBe(false)
    })
})
