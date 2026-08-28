// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const controllerMocks = vi.hoisted(() => {
    const listeners = new Set<() => void>()
    let operationPhase: 'idle' | 'awaitingConflict' | 'localCommitted' | 'sourceUnavailable' | 'sourcePrepared' | 'targetPrepared' | 'refreshPending' | 'completed' = 'idle'
    let sourcePhase: 'stopped' | 'prepared' | 'running' = 'stopped'
    let sourceBusy = false
    let revoked = false
    const snapshot = () => ({
        capabilities: {
            desktop: true as const,
            sourceReady: true,
            atomicActivationReady: true,
            authenticatedTransportReady: true,
            losslessBackupReady: true,
            durableStateReady: true,
            productionEnabled: true,
        },
        sourceStatus: {
            phase: sourcePhase,
            sessionId: sourcePhase === 'stopped' ? undefined : 'session-rehost',
            devices: [{
                deviceId: 'device-offline',
                transferredBytes: 14,
                lastSeenAt: 1,
                revoked,
            }],
        },
        sourcePairingUri: '',
        operationPhase,
        operationResult: operationPhase === 'awaitingConflict'
            ? {
                  kind: 'conflict' as const,
                  operationId: 'operation-retained',
                  conflicts: [{ key: 'r1:root', type: 'sameRecord' as const }],
                  localManifestHash: 'a'.repeat(64),
                  remoteManifestHash: 'b'.repeat(64),
              }
            : operationPhase === 'localCommitted'
            ? {
                  kind: 'resumeRequired' as const,
                  operationId: 'operation-retained',
                  phase: 'localCommitted' as const,
                  committedRevision: 7,
              }
            : operationPhase === 'sourceUnavailable'
                ? {
                      kind: 'sourceUnavailable' as const,
                      operationId: 'operation-retained',
                      committedRevision: 7,
                  }
                : operationPhase === 'completed' || operationPhase === 'refreshPending'
                    ? {
                          kind: 'noChanges' as const,
                          operationId: 'operation-retained',
                          revision: 7,
                          remoteRevision: 7,
                          transferredObjects: 0,
                          transferredBytes: 0,
                          backups: [],
                      }
                    : undefined,
        operationRetained: operationPhase !== 'idle',
        sourceBusy,
        sourceError: '',
        operationError: '',
    })
    const bidirectional = {
        snapshot,
        subscribe(listener: () => void) {
            listeners.add(listener)
            listener()
            return () => listeners.delete(listener)
        },
        initialize: vi.fn(async () => undefined),
        prepare: vi.fn(async () => {
            sourcePhase = 'prepared'
            for (const listener of listeners) listener()
        }),
        start: vi.fn(async () => {
            sourcePhase = 'running'
            for (const listener of listeners) listener()
        }),
        sync: vi.fn(async () => undefined),
        resume: vi.fn(async () => undefined),
        abandon: vi.fn(async () => undefined),
        revoke: vi.fn(async (_sessionId: string, deviceId: string) => {
            if (deviceId === 'device-offline') revoked = true
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
    return {
        bidirectional,
        clone,
        delta,
        reset() {
            operationPhase = 'idle'
            sourcePhase = 'stopped'
            sourceBusy = false
            revoked = false
        },
        setOperationPhase(phase: 'awaitingConflict' | 'localCommitted' | 'sourceUnavailable' | 'sourcePrepared' | 'targetPrepared' | 'refreshPending' | 'completed') {
            operationPhase = phase
        },
        setSourcePhase(phase: 'stopped' | 'prepared' | 'running') {
            sourcePhase = phase
        },
        setSourceBusy(value: boolean) {
            sourceBusy = value
            for (const listener of listeners) listener()
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

describe('peer bidirectional offline revoke', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        controllerMocks.reset()
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

    it('shows transferred bytes for a bidirectional source device', async () => {
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(PeerCloneSettings, { target })
        await tick()

        expect(target.textContent).toContain('device-offline (14 bytes)')
    })

    it('keeps source actions disabled across remount while the controller reports source busy', async () => {
        controllerMocks.setSourcePhase('prepared')
        controllerMocks.setSourceBusy(true)
        const target = document.createElement('div')
        document.body.append(target)
        const assertSourceActionsDisabled = () => {
            const button = (label: string) => [...target.querySelectorAll<HTMLButtonElement>('button')]
                .find((candidate) => candidate.textContent?.trim() === label)
            expect(button('Prepare pairing')?.disabled).toBe(true)
            expect(button('Start pairing')?.disabled).toBe(true)
            expect(button('Stop pairing')?.disabled).toBe(true)
        }

        mounted = mount(PeerCloneSettings, { target })
        await tick()
        assertSourceActionsDisabled()
        await unmount(mounted)
        mounted = undefined
        target.replaceChildren()

        mounted = mount(PeerCloneSettings, { target })
        await tick()
        assertSourceActionsDisabled()
    })

    it.each(['localCommitted', 'sourceUnavailable', 'targetPrepared', 'awaitingConflict'] as const)(
        'submits a fresh pairing URI while %s is retained',
        async (operationPhase) => {
            controllerMocks.setOperationPhase(operationPhase)
            const target = document.createElement('div')
            document.body.append(target)
            mounted = mount(PeerCloneSettings, { target })
            await tick()
            const pairingUri = 'risuailocal://peer-sync/v1?endpoint=http%3A%2F%2F192.168.1.20%3A32146'
                + '&session=123e4567-e89b-42d3-a456-426614174000'
                + `&manifest=${'a'.repeat(64)}#claim=${'b'.repeat(64)}`
            const input = target.querySelector<HTMLTextAreaElement>('#peer-bidirectional-target-uri')!
            input.value = pairingUri
            input.dispatchEvent(new Event('input', { bubbles: true }))
            await tick()
            const sync = [...target.querySelectorAll<HTMLButtonElement>('button')]
                .find((button) => button.textContent?.trim() === 'Sync both devices')

            expect(sync?.disabled).toBe(false)
            sync?.click()
            await vi.waitFor(() => expect(controllerMocks.bidirectional.sync).toHaveBeenCalledWith(pairingUri))
        },
    )

    it('offers resume and abandon for a target-prepared operation', async () => {
        controllerMocks.setOperationPhase('targetPrepared')
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(PeerCloneSettings, { target })
        await tick()
        const button = (label: string) => [...target.querySelectorAll<HTMLButtonElement>('button')]
            .find((candidate) => candidate.textContent?.trim() === label)

        expect(button('Resume sync')?.disabled).toBe(false)
        expect(button('Abandon pending sync')?.disabled).toBe(false)
        button('Resume sync')?.click()
        await vi.waitFor(() => expect(controllerMocks.bidirectional.resume).toHaveBeenCalledTimes(1))
        await vi.waitFor(() => expect(button('Abandon pending sync')?.disabled).toBe(false))
        button('Abandon pending sync')?.click()
        await vi.waitFor(() => expect(controllerMocks.bidirectional.abandon).toHaveBeenCalledTimes(1))
    })

    it('offers abandon and reconnection guidance for an awaiting conflict', async () => {
        controllerMocks.setOperationPhase('awaitingConflict')
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(PeerCloneSettings, { target })
        await tick()

        expect(target.textContent).toContain('Reconnect a restarted peer with a fresh link before choosing a version.')
        expect(target.textContent).toContain('The losing version receives a verified lossless backup before replacement.')
        const abandon = [...target.querySelectorAll<HTMLButtonElement>('button')]
            .find((button) => button.textContent?.trim() === 'Abandon pending sync')
        expect(abandon?.disabled).toBe(false)
        abandon?.click()
        await vi.waitFor(() => expect(controllerMocks.bidirectional.abandon).toHaveBeenCalledTimes(1))
    })

    it.each(['completed', 'sourcePrepared'] as const)(
        'prepares and starts a source while %s is retained',
        async (operationPhase) => {
            controllerMocks.setOperationPhase(operationPhase)
            const target = document.createElement('div')
            document.body.append(target)
            mounted = mount(PeerCloneSettings, { target })
            await tick()
            const button = (label: string) => [...target.querySelectorAll<HTMLButtonElement>('button')]
                .find((candidate) => candidate.textContent?.trim() === label)

            expect(button('Prepare pairing')?.disabled).toBe(false)
            button('Prepare pairing')?.click()
            await vi.waitFor(() => expect(controllerMocks.bidirectional.prepare).toHaveBeenCalledTimes(1))
            await vi.waitFor(() => expect(button('Start pairing')?.disabled).toBe(false))
            button('Start pairing')?.click()
            await vi.waitFor(() => expect(controllerMocks.bidirectional.start).toHaveBeenCalledWith('session-rehost'))
        },
    )

    it('offers abandon for a stopped source-prepared operation', async () => {
        controllerMocks.setOperationPhase('sourcePrepared')
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(PeerCloneSettings, { target })
        await tick()
        const abandon = [...target.querySelectorAll<HTMLButtonElement>('button')]
            .find((button) => button.textContent?.trim() === 'Abandon pending sync')

        expect(abandon?.disabled).toBe(false)
        abandon?.click()
        await vi.waitFor(() => expect(controllerMocks.bidirectional.abandon).toHaveBeenCalledTimes(1))
    })

    it.each(['prepared', 'running'] as const)(
        'hides source-prepared abandon while the source is %s',
        async (sourcePhase) => {
            controllerMocks.setOperationPhase('sourcePrepared')
            controllerMocks.setSourcePhase(sourcePhase)
            const target = document.createElement('div')
            document.body.append(target)
            mounted = mount(PeerCloneSettings, { target })
            await tick()

            expect([...target.querySelectorAll<HTMLButtonElement>('button')]
                .some((button) => button.textContent?.trim() === 'Abandon pending sync')).toBe(false)
        },
    )

    it.each(['awaitingConflict', 'targetPrepared', 'localCommitted', 'sourceUnavailable', 'refreshPending'] as const)(
        'keeps source prepare disabled while %s is retained',
        async (operationPhase) => {
            controllerMocks.setOperationPhase(operationPhase)
            const target = document.createElement('div')
            document.body.append(target)
            mounted = mount(PeerCloneSettings, { target })
            await tick()
            const prepare = [...target.querySelectorAll<HTMLButtonElement>('button')]
                .find((button) => button.textContent?.trim() === 'Prepare pairing')

            expect(prepare?.disabled).toBe(true)
        },
    )
})
