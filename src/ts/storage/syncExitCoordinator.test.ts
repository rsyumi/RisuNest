import { describe, expect, it, vi } from 'vitest'
import {
    createSyncExitCoordinator,
    type SyncExitDrainAdapter,
    type SyncExitTarget,
} from './syncExitCoordinator'

const target: SyncExitTarget = {
    revision: 12,
    libraryEpoch: 'epoch-1',
    selectionEpoch: 'selection-1',
    selectionId: 'server-1',
}

function deferred<T>() {
    let resolve!: (value: T) => void
    return {
        promise: new Promise<T>((settle) => { resolve = settle }),
        resolve,
    }
}

function harness(adapter: SyncExitDrainAdapter | null = null) {
    const order: string[] = []
    const fence = { release: vi.fn(() => order.push('release')) }
    const dependencies = {
        acquireEditFence: vi.fn(async () => {
            order.push('fence')
            return fence
        }),
        flushLocal: vi.fn(async () => { order.push('flush') }),
        checkpointLocal: vi.fn(async () => { order.push('checkpoint') }),
        captureTarget: vi.fn(async () => {
            order.push('capture')
            return target
        }),
        selectedDrain: vi.fn(() => adapter),
        softWaitMillis: 5_000,
        reportError: vi.fn(),
    }
    const coordinator = createSyncExitCoordinator(dependencies)
    return { coordinator, dependencies, fence, order }
}

describe('sync exit coordinator', () => {
    it('fences edits, flushes locally, then captures the exit target', async () => {
        const h = harness()

        await expect(h.coordinator.requestExit()).resolves.toBe('exit')

        expect(h.order).toEqual(['flush', 'checkpoint', 'fence', 'capture', 'release'])
        expect(h.coordinator.snapshot()).toEqual({ phase: 'complete', target })
    })

    it('coalesces duplicate exit requests and drains the captured target', async () => {
        const pending = deferred<{ kind: 'complete' }>()
        const adapter = {
            id: 'server',
            drain: vi.fn(() => pending.promise),
            cancel: vi.fn(async () => {}),
        }
        const h = harness(adapter)

        const first = h.coordinator.requestExit()
        const second = h.coordinator.requestExit()

        expect(second).toBe(first)
        await vi.waitFor(() => expect(adapter.drain).toHaveBeenCalledWith(
            target,
            expect.any(AbortSignal),
        ))
        pending.resolve({ kind: 'complete' })
        await expect(first).resolves.toBe('exit')
        expect(h.dependencies.captureTarget).toHaveBeenCalledOnce()
    })

    it('keeps local save failure separate and retries only when asked to wait', async () => {
        const h = harness()
        h.dependencies.flushLocal
            .mockRejectedValueOnce(new Error('disk full'))
            .mockResolvedValueOnce(undefined)

        const exit = h.coordinator.requestExit()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'local-failed',
        }))
        expect(h.dependencies.captureTarget).not.toHaveBeenCalled()
        expect(h.dependencies.reportError).toHaveBeenCalledOnce()
        expect(h.coordinator.decide('wait')).toBe(true)

        await expect(exit).resolves.toBe('exit')
        expect(h.dependencies.flushLocal).toHaveBeenCalledTimes(2)
    })

    it('can exit without saving after local failure without acquiring an edit fence', async () => {
        const h = harness()
        h.dependencies.flushLocal.mockRejectedValue(new Error('disk full'))
        const exit = h.coordinator.requestExit()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'local-failed',
        }))

        expect(h.coordinator.decide('exit-unsynced')).toBe(true)
        await expect(exit).resolves.toBe('exit')
        expect(h.dependencies.acquireEditFence).not.toHaveBeenCalled()
        expect(h.dependencies.captureTarget).not.toHaveBeenCalled()
    })

    it.each([
        'PersistentMutationFencedError',
        'SelectedConversationTransitionInProgressError',
    ])('does not bypass active %s when local flush is blocked', async (name) => {
        const h = harness()
        const fenced = new Error('An edit is active')
        fenced.name = name
        h.dependencies.flushLocal
            .mockRejectedValueOnce(fenced)
            .mockResolvedValueOnce(undefined)

        const exit = h.coordinator.requestExit()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'edit-blocked',
        }))
        expect(h.coordinator.decide('exit-unsynced')).toBe(true)
        await vi.waitFor(() => expect(h.dependencies.flushLocal).toHaveBeenCalledTimes(2))
        await expect(exit).resolves.toBe('exit')
        expect(h.dependencies.acquireEditFence).toHaveBeenCalledOnce()
    })

    it('keeps the exit pending when an edit or replacement fence is busy', async () => {
        const h = harness()
        h.dependencies.acquireEditFence
            .mockRejectedValueOnce(new Error('active import'))
            .mockResolvedValueOnce(h.fence)

        const exit = h.coordinator.requestExit()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'edit-blocked',
        }))
        expect(h.dependencies.flushLocal).toHaveBeenCalledOnce()

        h.coordinator.decide('wait')
        await expect(exit).resolves.toBe('exit')
        expect(h.dependencies.acquireEditFence).toHaveBeenCalledTimes(2)
    })

    it('does not bypass an active edit or replacement fence as an unsynced exit', async () => {
        const h = harness()
        h.dependencies.acquireEditFence.mockRejectedValue(new Error('active import'))

        const exit = h.coordinator.requestExit()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'edit-blocked',
        }))
        expect(h.coordinator.decide('exit-unsynced')).toBe(true)
        await vi.waitFor(() => expect(
            h.dependencies.acquireEditFence,
        ).toHaveBeenCalledTimes(2))
        expect(h.coordinator.snapshot()).toMatchObject({ phase: 'edit-blocked' })
        expect(h.coordinator.decide('cancel-exit')).toBe(true)

        await expect(exit).resolves.toBe('cancelled')
    })

    it('treats selection capture failure as remote blocking after local save succeeds', async () => {
        const h = harness()
        h.dependencies.captureTarget
            .mockRejectedValueOnce({ code: 'selection-changed' })
            .mockResolvedValueOnce(target)

        const exit = h.coordinator.requestExit()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'remote-blocked',
            target: null,
            reason: 'selection-changed',
        }))
        expect(h.dependencies.flushLocal).toHaveBeenCalledOnce()

        h.coordinator.decide('wait')
        await expect(exit).resolves.toBe('exit')
        expect(h.dependencies.flushLocal).toHaveBeenCalledOnce()
        expect(h.dependencies.captureTarget).toHaveBeenCalledTimes(2)
    })

    it('never exits at the soft wait boundary and can continue the same drain', async () => {
        vi.useFakeTimers()
        const pending = deferred<{ kind: 'complete' }>()
        const adapter = {
            id: 'external',
            drain: vi.fn(() => pending.promise),
            cancel: vi.fn(async () => {}),
        }
        const h = harness(adapter)
        const exit = h.coordinator.requestExit()
        await vi.advanceTimersByTimeAsync(5_000)

        expect(h.coordinator.snapshot()).toMatchObject({ phase: 'remote-delayed' })
        expect(h.coordinator.decide('wait')).toBe(true)
        expect(adapter.drain).toHaveBeenCalledOnce()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'syncing',
        }))
        await vi.advanceTimersByTimeAsync(5_000)
        expect(h.coordinator.snapshot()).toMatchObject({ phase: 'remote-delayed' })
        expect(adapter.drain).toHaveBeenCalledOnce()

        pending.resolve({ kind: 'complete' })
        await expect(exit).resolves.toBe('exit')
        vi.useRealTimers()
    })

    it('keeps blocked choices active when a delayed drain settles', async () => {
        vi.useFakeTimers()
        const pending = deferred<{ kind: 'blocked'; reason: string }>()
        const adapter = {
            id: 'external',
            drain: vi.fn(() => pending.promise),
            cancel: vi.fn(async () => {}),
        }
        const h = harness(adapter)
        const exit = h.coordinator.requestExit()
        await vi.advanceTimersByTimeAsync(5_000)
        expect(h.coordinator.snapshot()).toMatchObject({ phase: 'remote-delayed' })

        pending.resolve({ kind: 'blocked', reason: 'offline' })
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'remote-blocked',
            reason: 'offline',
        }))
        expect(h.coordinator.decide('exit-unsynced')).toBe(true)

        await expect(exit).resolves.toBe('exit')
        expect(adapter.cancel).toHaveBeenCalledOnce()
        vi.useRealTimers()
    })

    it.each([
        ['exit-unsynced', 'exit'],
        ['cancel-exit', 'cancelled'],
    ] as const)(
        'cancels a delayed drain for %s and releases the editing fence',
        async (choice, expected) => {
            vi.useFakeTimers()
            const adapter = {
                id: 'external-sequential',
                drain: vi.fn((_target: SyncExitTarget, signal: AbortSignal) =>
                    new Promise<{ kind: 'complete' }>((_resolve, reject) => {
                        signal.addEventListener('abort', () => reject(
                            new DOMException('aborted', 'AbortError'),
                        ))
                    })),
                cancel: vi.fn(async () => {}),
            }
            const h = harness(adapter)
            const exit = h.coordinator.requestExit()
            await vi.advanceTimersByTimeAsync(5_000)

            expect(h.coordinator.decide(choice)).toBe(true)
            await expect(exit).resolves.toBe(expected)
            expect(adapter.cancel).toHaveBeenCalledExactlyOnceWith(choice)
            expect(h.fence.release).toHaveBeenCalledOnce()
            vi.useRealTimers()
        },
    )

    it('retries a blocked remote drain without recapturing an older target', async () => {
        const adapter = {
            id: 'server',
            drain: vi.fn()
                .mockResolvedValueOnce({ kind: 'blocked' as const, reason: 'offline' })
                .mockResolvedValueOnce({ kind: 'complete' as const }),
            cancel: vi.fn(async () => {}),
        }
        const h = harness(adapter)
        const exit = h.coordinator.requestExit()
        await vi.waitFor(() => expect(h.coordinator.snapshot()).toMatchObject({
            phase: 'remote-blocked',
            reason: 'offline',
        }))

        h.coordinator.decide('wait')
        await expect(exit).resolves.toBe('exit')
        expect(adapter.drain).toHaveBeenCalledTimes(2)
        expect(adapter.drain.mock.calls[0][0]).toBe(target)
        expect(adapter.drain.mock.calls[1][0]).toBe(target)
        expect(h.dependencies.captureTarget).toHaveBeenCalledOnce()
    })
})
