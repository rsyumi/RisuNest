// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const controllerMocks = vi.hoisted(() => {
    const listeners = new Set<() => void>()
    let snapshot = {
        capabilities: undefined,
        sourceStatus: {
            phase: 'stopped' as const,
            devices: [{
                deviceId: 'device-offline',
                transferredBytes: 14,
                lastSeenAt: 1,
                revoked: false,
            }],
        },
        sourcePairingUri: '',
        operationPhase: 'idle' as const,
        operationResult: undefined,
        operationRetained: false,
        sourceError: '',
        operationError: '',
    }
    const bidirectional = {
        snapshot: () => snapshot,
        subscribe(listener: () => void) {
            listeners.add(listener)
            listener()
            return () => listeners.delete(listener)
        },
        initialize: vi.fn(async () => undefined),
        revoke: vi.fn(async (_sessionId: string, deviceId: string) => {
            snapshot = {
                ...snapshot,
                sourceStatus: {
                    ...snapshot.sourceStatus,
                    devices: snapshot.sourceStatus.devices.map((device) => (
                        device.deviceId === deviceId ? { ...device, revoked: true } : device
                    )),
                },
            }
            for (const listener of listeners) listener()
        }),
    }
    const clone = {
        snapshot: () => ({ sourcePairingUri: '' }),
        subscribe: () => () => undefined,
        initialize: vi.fn(async () => undefined),
    }
    const delta = {
        subscribe: () => () => undefined,
        initialize: vi.fn(async () => undefined),
    }
    return { bidirectional, clone, delta }
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

describe('peer bidirectional offline revoke', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    it('revokes a durable device without an active source session', async () => {
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(PeerCloneSettings, { target })
        await tick()
        const revoke = [...target.querySelectorAll<HTMLButtonElement>('button')]
            .find((button) => button.textContent?.trim() === 'Revoke')

        expect(revoke?.disabled).toBe(false)
        revoke?.click()
        await vi.waitFor(() => expect(controllerMocks.bidirectional.revoke).toHaveBeenCalledWith(
            '',
            'device-offline',
        ))
        await tick()
        expect(revoke?.disabled).toBe(true)
    })
})
