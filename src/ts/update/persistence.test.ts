import { describe, expect, it, vi } from 'vitest'
import { prepareUpdateInstallation } from './persistence'

function fixture() {
    const token = { revision: 7, mutationGeneration: 3 }
    const fence = { revision: 7, release: vi.fn(), refreshCommittedWorkingSet: vi.fn() }
    const runtime = {
        flushPendingDataLocally: vi.fn(async () => undefined),
        capturePersistentMutationToken: vi.fn(async () => token),
        acquireDestructiveReplacementFence: vi.fn(async () => fence),
    }
    return { runtime, token, fence }
}

describe('update installation persistence', () => {
    it('flushes locally and acquires the existing fence without publishing remotely', async () => {
        const { runtime, token, fence } = fixture()
        let finishFlush!: () => void
        runtime.flushPendingDataLocally.mockImplementation(() => new Promise<void>(resolve => { finishFlush = resolve }))
        const pending = prepareUpdateInstallation(runtime)
        expect(runtime.capturePersistentMutationToken).not.toHaveBeenCalled()
        expect(runtime.acquireDestructiveReplacementFence).not.toHaveBeenCalled()
        finishFlush()
        expect(await pending).toBe(fence)
        expect(runtime.flushPendingDataLocally).toHaveBeenCalledWith('app-update-install')
        expect(runtime.capturePersistentMutationToken).toHaveBeenCalledWith('app-update-install', { publishOfficial: false })
        expect(runtime.acquireDestructiveReplacementFence).toHaveBeenCalledWith(token)
        expect(fence.release).not.toHaveBeenCalled()
    })

    it.each(['flushPendingDataLocally', 'capturePersistentMutationToken', 'acquireDestructiveReplacementFence'] as const)(
        'propagates %s failure without pretending installation is safe', async (step) => {
            const { runtime, fence } = fixture()
            runtime[step].mockRejectedValue(new Error('synthetic persistence failure'))
            await expect(prepareUpdateInstallation(runtime)).rejects.toThrow('synthetic persistence failure')
            expect(fence.release).not.toHaveBeenCalled()
            if (step !== 'acquireDestructiveReplacementFence') {
                expect(runtime.acquireDestructiveReplacementFence).not.toHaveBeenCalled()
            }
        },
    )
})
