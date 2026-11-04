// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'

const { checkpointNativePersistentStore } = vi.hoisted(() => ({
    checkpointNativePersistentStore: vi.fn(async () => undefined),
}))

vi.mock('./persistentDataRuntime.svelte', () => ({
    flushPendingData: vi.fn(async () => undefined),
}))
vi.mock('./nativePersistentMaintenance', () => ({ checkpointNativePersistentStore }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))

import { registerLifecycleCommitListeners } from './lifecycleCommit'
import { createLifecycleFlush } from './lifecycleFlush'
import { beginDisplayWork, installDisplayRecovery, suspendDisplayWork } from '../displayRecovery.svelte'
import { recoveryStore } from '../displayRecovery.testSupport'
import {
    captureRoot,
    deferred,
    makeDatabase,
    makeStore,
    SaveCoordinator,
} from './saveCoordinator.testSupport'

function platformLifecycleFlush(
    runtime: Pick<SaveCoordinator, 'flushPendingData' | 'flushPendingDataLocally'>,
    desktop: boolean,
    platform: string,
) {
    return createLifecycleFlush({
        runtime,
        isTauriAndroid: !desktop && platform === 'android',
        isTauriDesktop: desktop,
        nativePlatform: () => platform,
        forgetInlayProviderImages: vi.fn(),
        releaseIdleTransformerModels: vi.fn(async () => undefined),
        suspendDisplayWork,
    }).flushForLifecycle
}

const originalVisibilityState = Object.getOwnPropertyDescriptor(document, 'visibilityState')

function setVisibilityState(value: DocumentVisibilityState): void {
    Object.defineProperty(document, 'visibilityState', {
        configurable: true,
        value,
    })
}

afterEach(() => {
    if (originalVisibilityState) {
        Object.defineProperty(document, 'visibilityState', originalVisibilityState)
    } else {
        Reflect.deleteProperty(document, 'visibilityState')
    }
    vi.restoreAllMocks()
})

describe('registerLifecycleCommitListeners', () => {
    it.each([
        [true, 'windows', 'stop', true],
        [true, 'windows', 'exit', false],
        [true, 'windows', 'trim-memory', false],
        [true, 'windows', 'pagehide', false],
        [true, 'windows', 'visibility-hidden', false],
        [true, 'macos', 'stop', false],
        [true, 'linux', 'stop', true],
        [true, 'linux', 'exit', false],
        [false, 'android', 'stop', true],
        [false, 'android', 'trim-memory', true],
        [false, 'android', 'exit', false],
        [false, 'android', 'pagehide', false],
        [false, 'android', 'visibility-hidden', false],
        [false, 'ios', 'stop', false],
        [false, 'windows', 'stop', false],
    ] as const)('uses the platform lifecycle persistence policy (desktop=%s, platform=%s, reason=%s)', async (
        desktop, platform, reason, locally,
    ) => {
        const runtime = {
            flushPendingData: vi.fn(async () => undefined),
            flushPendingDataLocally: vi.fn(async () => undefined),
        }
        await platformLifecycleFlush(runtime, desktop, platform)(reason)
        expect(runtime.flushPendingData.mock.calls).toEqual(locally ? [] : [[reason]])
        expect(runtime.flushPendingDataLocally.mock.calls).toEqual(locally ? [[reason]] : [])
    })

    it('commits a newer Windows stop edit and acknowledges before a pending official publication settles', async () => {
        const database = makeDatabase()
        const publication = deferred<void>()
        const checkpointDone = deferred<void>()
        const order: string[] = []
        const publish = vi.fn(() => publication.promise)
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        const commit = vi.fn(async ({ expectedRevision }) => {
            order.push(`commit-${expectedRevision + 1}`)
            return { revision: expectedRevision + 1 }
        })
        const coordinator = new SaveCoordinator({
            store: makeStore(commit),
            captureRoot: () => captureRoot(database),
            captureSelectedCharacter: () => database.characters[0],
            replaceDatabase: () => undefined,
            officialPublisher: { pin },
            clock: { setTimeout: () => undefined, clearTimeout: () => undefined },
        })
        coordinator.initialize(1)
        database.username = 'Synthetic published edit'
        coordinator.markPersistentDataDirty(1)
        const ordinaryFlush = coordinator.flushPendingData('ordinary-save')
        await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
        database.username = 'Synthetic Windows shutdown edit'
        coordinator.markPersistentDataDirty(1)
        const onFlushComplete = vi.fn(() => { order.push('ack') })
        const exitCoordinator = { requestExit: vi.fn(async () => 'exit' as const) }
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const checkpoint = vi.fn(() => {
            order.push('checkpoint')
            return checkpointDone.promise
        })
        const dispose = registerLifecycleCommitListeners(
            platformLifecycleFlush(coordinator, true, 'windows'),
            exitCoordinator,
            checkpoint,
        )
        try {
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
                detail: { reason: 'stop', ackToken: 'windows-session-end' },
            }))
            await vi.waitFor(() => expect(checkpoint).toHaveBeenCalledWith('truncate'))
            expect(commit).toHaveBeenCalledTimes(2)
            expect(commit).toHaveBeenNthCalledWith(2, expect.objectContaining({
                expectedRevision: 2,
                rootMutations: [{ type: 'set', key: 'username', value: 'Synthetic Windows shutdown edit' }],
            }))
            expect(coordinator.revision).toBe(3)
            expect(onFlushComplete).not.toHaveBeenCalled()
            checkpointDone.resolve()
            await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledExactlyOnceWith('windows-session-end'))
            expect(order).toEqual(['commit-2', 'commit-3', 'checkpoint', 'ack'])
            expect(coordinator.hasPendingOfficialPublication).toBe(true)
            expect(pin).toHaveBeenCalledExactlyOnceWith(2)
            expect(exitCoordinator.requestExit).not.toHaveBeenCalled()
        } finally {
            checkpointDone.resolve()
            publication.reject(new Error('Synthetic publication teardown'))
            await ordinaryFlush.catch(() => undefined)
            dispose()
            delete (window as any).RisuLifecycleBridge
        }
    })

    it.each(['stop', 'trim-memory'] as const)(
        'commits a newer Android %s edit while an official publication is pending',
        async (reason) => {
            const database = makeDatabase()
            const publication = deferred<void>()
            const order: string[] = []
            const publish = vi.fn(() => publication.promise)
            const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
            const commit = vi.fn(async ({ expectedRevision }) => {
                order.push(`commit-${expectedRevision + 1}`)
                return { revision: expectedRevision + 1 }
            })
            const coordinator = new SaveCoordinator({
                store: makeStore(commit),
                captureRoot: () => captureRoot(database),
                captureSelectedCharacter: () => database.characters[0],
                replaceDatabase: () => undefined,
                officialPublisher: { pin },
                clock: { setTimeout: () => undefined, clearTimeout: () => undefined },
            })
            coordinator.initialize(1)
            database.username = 'Synthetic published edit'
            coordinator.markPersistentDataDirty(1)
            const ordinaryFlush = coordinator.flushPendingData('ordinary-save')
            await vi.waitFor(() => expect(publish).toHaveBeenCalledOnce())
            database.username = 'Synthetic Android background edit'
            coordinator.markPersistentDataDirty(1)
            const checkpoint = vi.fn(async () => { order.push('checkpoint') })
            const dispose = registerLifecycleCommitListeners(
                platformLifecycleFlush(coordinator, false, 'android'),
                undefined,
                checkpoint,
            )
            try {
                window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail: { reason } }))
                await vi.waitFor(() => expect(checkpoint).toHaveBeenCalledWith('truncate'))
                expect(commit).toHaveBeenNthCalledWith(2, expect.objectContaining({
                    expectedRevision: 2,
                    rootMutations: [{ type: 'set', key: 'username', value: 'Synthetic Android background edit' }],
                }))
                expect(order).toEqual(['commit-2', 'commit-3', 'checkpoint'])
                expect(coordinator.hasPendingOfficialPublication).toBe(true)
                expect(pin).toHaveBeenCalledExactlyOnceWith(2)
            } finally {
                publication.reject(new Error('Synthetic publication teardown'))
                await ordinaryFlush.catch(() => undefined)
                dispose()
            }
        },
    )

    it('flushes every pagehide event', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true }))

        expect(flush).toHaveBeenCalledWith('pagehide')
        dispose()
    })

    it('flushes when the document becomes hidden', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)
        setVisibilityState('hidden')

        document.dispatchEvent(new Event('visibilitychange'))

        expect(flush).toHaveBeenCalledWith('visibility-hidden')
        dispose()
    })

    it('does not flush while the document is visible', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)
        setVisibilityState('visible')

        document.dispatchEvent(new Event('visibilitychange'))

        expect(flush).not.toHaveBeenCalled()
        dispose()
    })

    it.each(['stop', 'trim-memory', 'exit'] as const)('forwards native %s events', (reason) => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason },
        }))

        expect(flush).toHaveBeenCalledWith(reason)
        dispose()
    })

    it.each([
        undefined,
        null,
        {},
        { reason: 'unknown' },
        { reason: 1 },
    ])('ignores malformed native lifecycle payload %#', (detail) => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail }))

        expect(flush).not.toHaveBeenCalled()
        dispose()
    })

    it('forwards close-together eligible events independently', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new Event('pagehide'))
        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'stop' },
        }))

        expect(flush.mock.calls).toEqual([['pagehide'], ['stop']])
        dispose()
    })

    it('acknowledges the native ack token after the flush settles', async () => {
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        let resolveFlush!: () => void
        const flush = vi.fn(() => new Promise<void>((resolve) => { resolveFlush = resolve }))
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit', ackToken: 'exit-1' },
        }))

        expect(flush).toHaveBeenCalledWith('exit')
        expect(onFlushComplete).not.toHaveBeenCalled()

        resolveFlush()
        await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('exit-1'))
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('acknowledges a native stop after the record of running display work is removed', async () => {
        const device = recoveryStore()
        await installDisplayRecovery(device.store)
        const work = beginDisplayWork({ characterId: 'synthetic-character', conversationId: 'synthetic-chat' })
        device.commitAll()
        await work.ready
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const runtime = {
            flushPendingData: vi.fn(async () => undefined),
            flushPendingDataLocally: vi.fn(async () => undefined),
        }
        const dispose = registerLifecycleCommitListeners(
            platformLifecycleFlush(runtime, false, 'android'),
            undefined,
            vi.fn(async () => undefined),
        )
        try {
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
                detail: { reason: 'stop', ackToken: 'stop-display' },
            }))
            await vi.waitFor(() => expect(device.pendingWrites).toBe(1))
            expect(device.record).toBeNull()
            // A turn of the event loop in which an acknowledgement that did not wait would arrive.
            await new Promise((resolve) => setTimeout(resolve, 0))
            expect(onFlushComplete).not.toHaveBeenCalled()

            device.commitAll()
            await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledExactlyOnceWith('stop-display'))
        } finally {
            dispose()
            work.release()
            await installDisplayRecovery(null)
            delete (window as any).RisuLifecycleBridge
        }
    })

    it('flushes, checkpoints with truncate, then acknowledges a native event', async () => {
        const order: string[] = []
        const onFlushComplete = vi.fn(() => order.push('ack'))
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const flush = vi.fn(async () => { order.push('flush') })
        const checkpoint = vi.fn(async () => { order.push('checkpoint') })
        const dispose = registerLifecycleCommitListeners(flush, undefined, checkpoint)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'stop', ackToken: 'stop-1' },
        }))

        await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('stop-1'))
        expect(order).toEqual(['flush', 'checkpoint', 'ack'])
        expect(checkpoint).toHaveBeenCalledWith('truncate')
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('attempts the checkpoint and acknowledges after a flush rejection', async () => {
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const flush = vi.fn(async () => { throw new Error('flush failed') })
        const checkpoint = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush, undefined, checkpoint)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'trim-memory', ackToken: 'trim-1' },
        }))

        await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('trim-1'))
        expect(checkpoint).toHaveBeenCalledWith('truncate')
        expect(errorLog).toHaveBeenCalledWith(
            'Lifecycle flush failed for trim-memory',
            expect.any(Error),
        )
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('logs a checkpoint rejection and still acknowledges', async () => {
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const checkpointError = new Error('checkpoint failed')
        const checkpoint = vi.fn(async () => { throw checkpointError })
        const dispose = registerLifecycleCommitListeners(
            vi.fn(async () => undefined),
            undefined,
            checkpoint,
        )

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit', ackToken: 'exit-checkpoint' },
        }))

        await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('exit-checkpoint'))
        expect(errorLog).toHaveBeenCalledWith(
            'Lifecycle checkpoint failed for exit',
            checkpointError,
        )
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('does not checkpoint on the web production path', async () => {
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const dispose = registerLifecycleCommitListeners(vi.fn(async () => undefined))

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'stop', ackToken: 'web-stop' },
        }))

        await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('web-stop'))
        expect(checkpointNativePersistentStore).not.toHaveBeenCalled()
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it.each(['stop', 'trim-memory', 'exit'] as const)('records the native %s reason and checkpoints on the Tauri production path', async (reason) => {
        checkpointNativePersistentStore.mockClear()
        vi.resetModules()
        vi.doMock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'tauri', os: 'windows' }))
        const diagnosticEvents: Record<string, unknown>[] = []
        vi.doMock('@tauri-apps/api/core', async (importOriginal) => ({
            ...await importOriginal<typeof import('@tauri-apps/api/core')>(),
            invoke: async (command: string, args?: { event?: Record<string, unknown> }) => {
                if (command === 'native_generation_diagnostic' && args?.event) diagnosticEvents.push(args.event)
                return 7
            },
        }))
        const { registerLifecycleCommitListeners: registerTauri } = await import('./lifecycleCommit')
        const { generationDiagnostics } = await import('../generationDiagnostics')
        generationDiagnostics.startDocument()
        await Promise.resolve()
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const dispose = registerTauri(vi.fn(async () => undefined))

        try {
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
                detail: { reason, ackToken: 'tauri-lifecycle' },
            }))

            await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('tauri-lifecycle'))
            expect(checkpointNativePersistentStore).toHaveBeenCalledWith('truncate')
            expect(diagnosticEvents).toEqual([
                { kind: 'document-start' },
                { kind: 'lifecycle', documentSequence: 7, eventSequence: 1, event: reason },
            ])
        } finally {
            dispose()
            delete (window as any).RisuLifecycleBridge
            vi.doUnmock('../platform')
            vi.doUnmock('@tauri-apps/api/core')
            vi.resetModules()
        }
    })

    it('records the return, the trim level and page freeze and resume on the Tauri production path without extra flushes', async () => {
        checkpointNativePersistentStore.mockClear()
        vi.resetModules()
        vi.doMock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'tauri', os: 'android' }))
        const diagnosticEvents: Record<string, unknown>[] = []
        vi.doMock('@tauri-apps/api/core', async (importOriginal) => ({
            ...await importOriginal<typeof import('@tauri-apps/api/core')>(),
            invoke: async (command: string, args?: { event?: Record<string, unknown> }) => {
                if (command === 'native_generation_diagnostic' && args?.event) diagnosticEvents.push(args.event)
                return 7
            },
        }))
        const { registerLifecycleCommitListeners: registerTauri } = await import('./lifecycleCommit')
        const { generationDiagnostics } = await import('../generationDiagnostics')
        generationDiagnostics.startDocument()
        await Promise.resolve()
        const flush = vi.fn(async () => undefined)
        const dispose = registerTauri(flush)

        try {
            document.dispatchEvent(new Event('freeze'))
            document.dispatchEvent(new Event('resume'))
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail: { reason: 'start' } }))
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail: { reason: 'trim-memory', trimLevel: 20 } }))
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail: { reason: 'stop', trimLevel: 20 } }))

            await vi.waitFor(() => expect(flush.mock.calls).toEqual([['trim-memory'], ['stop']]))
            expect(diagnosticEvents.slice(1)).toEqual([
                { kind: 'lifecycle', documentSequence: 7, eventSequence: 1, event: 'freeze' },
                { kind: 'lifecycle', documentSequence: 7, eventSequence: 2, event: 'resume' },
                { kind: 'lifecycle', documentSequence: 7, eventSequence: 3, event: 'start' },
                { kind: 'lifecycle', documentSequence: 7, eventSequence: 4, event: 'trim-memory', trimLevel: 20 },
                { kind: 'lifecycle', documentSequence: 7, eventSequence: 5, event: 'stop' },
            ])
        } finally {
            dispose()
            vi.doUnmock('../platform')
            vi.doUnmock('@tauri-apps/api/core')
            vi.resetModules()
        }
    })

    it('acknowledges the native ack token when the flush fails', async () => {
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const flush = vi.fn(async () => { throw new Error('flush failed') })
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit', ackToken: 'exit-2' },
        }))

        await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('exit-2'))
        errorLog.mockRestore()
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('holds explicit exit and offers retry when local flush fails', async () => {
        const bridge = {
            onFlushComplete: vi.fn(),
            onFlushHold: vi.fn(),
            requestExit: vi.fn(),
        }
        ;(window as any).RisuLifecycleBridge = bridge
        const flush = vi.fn()
            .mockRejectedValueOnce(new Error('flush failed'))
            .mockResolvedValueOnce(undefined)
        const confirmExitWithoutSaving = vi.fn(async () => false)
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const dispose = registerLifecycleCommitListeners(
            flush,
            undefined,
            undefined,
            confirmExitWithoutSaving,
        )

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit', ackToken: 'exit-retry' },
        }))

        await vi.waitFor(() => expect(flush).toHaveBeenCalledTimes(2))
        await vi.waitFor(() => expect(bridge.requestExit).toHaveBeenCalledTimes(1))
        expect(bridge.onFlushHold).toHaveBeenCalledWith('exit-retry')
        expect(confirmExitWithoutSaving).toHaveBeenCalledTimes(1)
        expect(bridge.onFlushComplete).not.toHaveBeenCalled()
        errorLog.mockRestore()
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('can exit without saving after an explicit exit checkpoint failure', async () => {
        const bridge = {
            onFlushComplete: vi.fn(),
            onFlushHold: vi.fn(),
            requestExit: vi.fn(),
        }
        ;(window as any).RisuLifecycleBridge = bridge
        const checkpoint = vi.fn(async () => { throw new Error('checkpoint failed') })
        const confirmExitWithoutSaving = vi.fn(async () => true)
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const dispose = registerLifecycleCommitListeners(
            vi.fn(async () => undefined),
            undefined,
            checkpoint,
            confirmExitWithoutSaving,
        )

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit', ackToken: 'exit-without-saving' },
        }))

        await vi.waitFor(() => expect(confirmExitWithoutSaving).toHaveBeenCalledTimes(1))
        expect(bridge.onFlushHold).toHaveBeenCalledWith('exit-without-saving')
        expect(bridge.requestExit).toHaveBeenCalledTimes(1)
        expect(bridge.onFlushComplete).not.toHaveBeenCalled()
        errorLog.mockRestore()
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('offers exit without saving when an explicit exit flush never settles', async () => {
        const bridge = {
            onFlushComplete: vi.fn(),
            onFlushHold: vi.fn(),
            requestExit: vi.fn(),
        }
        ;(window as any).RisuLifecycleBridge = bridge
        const flush = vi.fn(() => new Promise<void>(() => {}))
        const confirmExitWithoutSaving = vi.fn(async () => true)
        const settleTimeout = vi.fn(async () => {
            throw new Error('local settle timed out')
        })
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const dispose = registerLifecycleCommitListeners(
            flush,
            undefined,
            undefined,
            confirmExitWithoutSaving,
            settleTimeout,
        )

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit', ackToken: 'exit-timeout' },
        }))

        await vi.waitFor(() => expect(confirmExitWithoutSaving).toHaveBeenCalledTimes(1))
        expect(bridge.onFlushHold).toHaveBeenCalledWith('exit-timeout')
        expect(bridge.requestExit).toHaveBeenCalledTimes(1)
        expect(bridge.onFlushComplete).not.toHaveBeenCalled()
        errorLog.mockRestore()
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('offers exit without saving when an explicit exit checkpoint never settles', async () => {
        const bridge = {
            onFlushComplete: vi.fn(),
            onFlushHold: vi.fn(),
            requestExit: vi.fn(),
        }
        ;(window as any).RisuLifecycleBridge = bridge
        const checkpoint = vi.fn(() => new Promise<void>(() => {}))
        const confirmExitWithoutSaving = vi.fn(async () => true)
        let rejectTimeout!: (reason: unknown) => void
        const settleTimeout = vi.fn((
            _settlement: Promise<boolean>,
            _timeoutMillis: number,
        ) => new Promise<boolean>((_resolve, reject) => {
            rejectTimeout = reject
        }))
        const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
        const dispose = registerLifecycleCommitListeners(
            vi.fn(async () => undefined),
            undefined,
            checkpoint,
            confirmExitWithoutSaving,
            settleTimeout,
        )

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit', ackToken: 'checkpoint-timeout' },
        }))
        await vi.waitFor(() => expect(checkpoint).toHaveBeenCalledWith('truncate'))

        rejectTimeout(new Error('local settle timed out'))

        await vi.waitFor(() => expect(confirmExitWithoutSaving).toHaveBeenCalledTimes(1))
        expect(settleTimeout).toHaveBeenCalledWith(expect.any(Promise), 1_500)
        expect(bridge.requestExit).toHaveBeenCalledTimes(1)
        expect(bridge.onFlushComplete).not.toHaveBeenCalled()
        errorLog.mockRestore()
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    it('sends no acknowledgement without an ack token', async () => {
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'exit' },
        }))
        await Promise.resolve()
        await Promise.resolve()
        await Promise.resolve()

        expect(onFlushComplete).not.toHaveBeenCalled()
        dispose()
        delete (window as any).RisuLifecycleBridge
    })

    describe('exit sync policy', () => {
        function makeBridge() {
            const bridge = {
                onFlushComplete: vi.fn(),
                onFlushHold: vi.fn(),
                requestExit: vi.fn(),
            }
            ;(window as any).RisuLifecycleBridge = bridge
            return bridge
        }

        function dispatchExit(token = 'exit-1'): void {
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
                detail: { reason: 'exit', ackToken: token },
            }))
        }

        afterEach(() => {
            delete (window as any).RisuLifecycleBridge
        })

        it('holds the native exit, flushes, then exits when nothing is pending', async () => {
            const bridge = makeBridge()
            const flush = vi.fn(async () => undefined)
            const policy = {
                isSyncActive: () => true,
                hasPendingSync: vi.fn(() => false),
                confirmExit: vi.fn(async () => true),
            }
            const dispose = registerLifecycleCommitListeners(flush, policy)

            dispatchExit()

            expect(bridge.onFlushHold).toHaveBeenCalledWith('exit-1')
            expect(flush).toHaveBeenCalledWith('exit')
            await vi.waitFor(() => expect(bridge.requestExit).toHaveBeenCalledTimes(1))
            expect(policy.confirmExit).not.toHaveBeenCalled()
            expect(bridge.onFlushComplete).not.toHaveBeenCalled()
            dispose()
        })

        it('waits for the checkpoint before evaluating a held exit', async () => {
            const bridge = makeBridge()
            let settleCheckpoint!: () => void
            const checkpoint = vi.fn(() => new Promise<void>((resolve) => {
                settleCheckpoint = resolve
            }))
            const policy = {
                isSyncActive: () => true,
                hasPendingSync: vi.fn(() => false),
                confirmExit: vi.fn(async () => true),
            }
            const dispose = registerLifecycleCommitListeners(
                vi.fn(async () => undefined),
                policy,
                checkpoint,
            )

            dispatchExit()
            await vi.waitFor(() => expect(checkpoint).toHaveBeenCalledTimes(1))

            expect(policy.hasPendingSync).not.toHaveBeenCalled()
            expect(policy.confirmExit).not.toHaveBeenCalled()
            expect(bridge.requestExit).not.toHaveBeenCalled()

            settleCheckpoint()
            await vi.waitFor(() => expect(bridge.requestExit).toHaveBeenCalledTimes(1))
            dispose()
        })

        it('stays open when sync is pending and the user declines the exit', async () => {
            const bridge = makeBridge()
            const flush = vi.fn(async () => undefined)
            const policy = {
                isSyncActive: () => true,
                hasPendingSync: vi.fn(() => true),
                confirmExit: vi.fn(async () => false),
            }
            const dispose = registerLifecycleCommitListeners(flush, policy)

            dispatchExit()

            await vi.waitFor(() => expect(policy.confirmExit).toHaveBeenCalledTimes(1))
            expect(bridge.requestExit).not.toHaveBeenCalled()
            expect(bridge.onFlushComplete).not.toHaveBeenCalled()
            dispose()
        })

        it('exits when sync is pending but the user confirms', async () => {
            const bridge = makeBridge()
            const flush = vi.fn(async () => undefined)
            const policy = {
                isSyncActive: () => true,
                hasPendingSync: vi.fn(() => true),
                confirmExit: vi.fn(async () => true),
            }
            const dispose = registerLifecycleCommitListeners(flush, policy)

            dispatchExit()

            await vi.waitFor(() => expect(bridge.requestExit).toHaveBeenCalledTimes(1))
            dispose()
        })

        it('retries a failed local save before asking about pending sync', async () => {
            const bridge = makeBridge()
            const errorLog = vi.spyOn(console, 'error').mockImplementation(() => {})
            const flush = vi.fn()
                .mockRejectedValueOnce(new Error('flush failed'))
                .mockResolvedValueOnce(undefined)
            const policy = {
                isSyncActive: () => true,
                hasPendingSync: vi.fn(() => true),
                confirmExit: vi.fn(async () => false),
            }
            const confirmExitWithoutSaving = vi.fn(async () => false)
            const dispose = registerLifecycleCommitListeners(
                flush,
                policy,
                undefined,
                confirmExitWithoutSaving,
            )

            dispatchExit()

            await vi.waitFor(() => expect(policy.confirmExit).toHaveBeenCalledTimes(1))
            expect(flush).toHaveBeenCalledTimes(2)
            expect(confirmExitWithoutSaving).toHaveBeenCalledTimes(1)
            expect(bridge.requestExit).not.toHaveBeenCalled()
            errorLog.mockRestore()
            dispose()
        })

        it('holds and completes explicit exit when sync is inactive', async () => {
            const bridge = makeBridge()
            const flush = vi.fn(async () => undefined)
            const policy = {
                isSyncActive: () => false,
                hasPendingSync: vi.fn(() => true),
                confirmExit: vi.fn(async () => true),
            }
            const dispose = registerLifecycleCommitListeners(flush, policy)

            dispatchExit('exit-9')

            await vi.waitFor(() => expect(bridge.requestExit).toHaveBeenCalledTimes(1))
            expect(bridge.onFlushHold).toHaveBeenCalledWith('exit-9')
            expect(bridge.onFlushComplete).not.toHaveBeenCalled()
            dispose()
        })

        it('falls back to the acknowledge path when the bridge cannot hold the exit', async () => {
            const onFlushComplete = vi.fn()
            ;(window as any).RisuLifecycleBridge = { onFlushComplete }
            const flush = vi.fn(async () => undefined)
            const policy = {
                isSyncActive: () => true,
                hasPendingSync: vi.fn(() => true),
                confirmExit: vi.fn(async () => true),
            }
            const dispose = registerLifecycleCommitListeners(flush, policy)

            dispatchExit('exit-5')

            await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('exit-5'))
            expect(policy.confirmExit).not.toHaveBeenCalled()
            dispose()
        })
    })

    describe('coordinated exit drain', () => {
        afterEach(() => {
            delete (window as any).RisuLifecycleBridge
        })

        it('delegates the complete local and remote settlement and coalesces duplicate exits', async () => {
            const bridge = {
                onFlushComplete: vi.fn(),
                onFlushHold: vi.fn(),
                requestExit: vi.fn(),
            }
            ;(window as any).RisuLifecycleBridge = bridge
            let finish!: (result: 'exit' | 'cancelled') => void
            const coordinator = {
                requestExit: vi.fn(() => new Promise<'exit' | 'cancelled'>((resolve) => {
                    finish = resolve
                })),
            }
            const flush = vi.fn(async () => undefined)
            const dispose = registerLifecycleCommitListeners(flush, coordinator)

            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
                detail: { reason: 'exit', ackToken: 'exit-first' },
            }))
            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
                detail: { reason: 'exit', ackToken: 'exit-duplicate' },
            }))

            expect(coordinator.requestExit).toHaveBeenCalledOnce()
            expect(flush).not.toHaveBeenCalled()
            expect(bridge.requestExit).not.toHaveBeenCalled()
            finish('exit')
            await vi.waitFor(() => expect(bridge.requestExit).toHaveBeenCalledOnce())
            expect(bridge.onFlushHold.mock.calls).toEqual([['exit-first'], ['exit-duplicate']])
            dispose()
        })

        it('holds every repeated token while a cancelled decision and its timers settle', async () => {
            vi.useFakeTimers()
            const pending = new Set<string>()
            const nativeFinish = vi.fn()
            const bridge = {
                onFlushHold: vi.fn((token: string) => pending.delete(token)),
                requestExit: vi.fn(),
                exitListenerReady: vi.fn(),
            }
            ;(window as any).RisuLifecycleBridge = bridge
            let resolve!: (result: 'cancelled') => void
            const coordinator = { requestExit: vi.fn(() => new Promise<'cancelled'>(r => { resolve = r })) }
            const dispose = registerLifecycleCommitListeners(undefined, coordinator)
            expect(bridge.exitListenerReady).toHaveBeenCalledWith(true)
            for (const token of ['first', 'second']) {
                pending.add(token)
                setTimeout(() => { if (pending.delete(token)) nativeFinish() }, 1500)
                window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail: { reason: 'exit', ackToken: token } }))
            }
            resolve('cancelled')
            await vi.runAllTimersAsync()
            expect(coordinator.requestExit).toHaveBeenCalledOnce()
            expect(nativeFinish).not.toHaveBeenCalled()
            expect(bridge.requestExit).not.toHaveBeenCalled()
            dispose()
            expect(bridge.exitListenerReady).toHaveBeenLastCalledWith(false)
            vi.useRealTimers()
        })

        it('keeps the native window open when the coordinated drain cancels exit', async () => {
            const bridge = {
                onFlushHold: vi.fn(),
                requestExit: vi.fn(),
            }
            ;(window as any).RisuLifecycleBridge = bridge
            const coordinator = {
                requestExit: vi.fn(async () => 'cancelled' as const),
            }
            const dispose = registerLifecycleCommitListeners(undefined, coordinator)

            window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
                detail: { reason: 'exit', ackToken: 'exit-cancelled' },
            }))

            await vi.waitFor(() => expect(coordinator.requestExit).toHaveBeenCalledOnce())
            expect(bridge.requestExit).not.toHaveBeenCalled()
            dispose()
        })
    })

    it('removes all listeners and can be disposed twice', () => {
        const flush = vi.fn(async () => undefined)
        const dispose = registerLifecycleCommitListeners(flush)
        dispose()
        dispose()
        setVisibilityState('hidden')

        window.dispatchEvent(new Event('pagehide'))
        document.dispatchEvent(new Event('visibilitychange'))
        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'trim-memory' },
        }))

        expect(flush).not.toHaveBeenCalled()
    })
})
