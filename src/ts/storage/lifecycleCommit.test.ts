// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'

const { checkpointNativePersistentStore } = vi.hoisted(() => ({
    checkpointNativePersistentStore: vi.fn(async () => undefined),
}))

vi.mock('./persistentDataRuntime.svelte', () => ({
    flushPendingData: vi.fn(async () => undefined),
}))
vi.mock('./nativePersistentMaintenance', () => ({ checkpointNativePersistentStore }))
vi.mock('../platform', () => ({ isTauri: false }))

import { registerLifecycleCommitListeners } from './lifecycleCommit'

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

    it('checkpoints through the native store on the Tauri production path', async () => {
        checkpointNativePersistentStore.mockClear()
        vi.resetModules()
        vi.doMock('../platform', () => ({ isTauri: true }))
        const { registerLifecycleCommitListeners: registerTauri } = await import('./lifecycleCommit')
        const onFlushComplete = vi.fn()
        ;(window as any).RisuLifecycleBridge = { onFlushComplete }
        const dispose = registerTauri(vi.fn(async () => undefined))

        window.dispatchEvent(new CustomEvent('risu-native-lifecycle', {
            detail: { reason: 'stop', ackToken: 'tauri-stop' },
        }))

        await vi.waitFor(() => expect(onFlushComplete).toHaveBeenCalledWith('tauri-stop'))
        expect(checkpointNativePersistentStore).toHaveBeenCalledWith('truncate')
        dispose()
        delete (window as any).RisuLifecycleBridge
        vi.doUnmock('../platform')
        vi.resetModules()
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
