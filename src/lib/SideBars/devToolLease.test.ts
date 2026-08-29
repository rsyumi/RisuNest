import { describe, expect, test, vi } from 'vitest'

import { DevToolConversationLease } from './devToolLease'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((done) => { resolve = done })
    return { promise, resolve }
}

describe('DevTool complete conversation lease', () => {
    test('releases a panel lease that resolves after the panel is destroyed', async () => {
        const pending = deferred<any>()
        const release = vi.fn()
        const lifetime = new DevToolConversationLease()
        const acquiring = lifetime.acquire(
            { conversationId: 'chat-a' } as any,
            () => pending.promise,
        )

        lifetime.destroy()
        pending.resolve({ release })

        await expect(acquiring).resolves.toBe(false)
        expect(release).toHaveBeenCalledOnce()
    })

    test('holds an acquired lease until panel destruction and releases once', async () => {
        const release = vi.fn()
        const lifetime = new DevToolConversationLease()

        await expect(lifetime.acquire(
            { conversationId: 'chat-a' } as any,
            async () => ({ release }) as any,
        )).resolves.toBe(true)

        lifetime.destroy()
        lifetime.destroy()
        expect(release).toHaveBeenCalledOnce()
    })

    test('releases the old lease and holds the replacement when selection changes', async () => {
        const firstRelease = vi.fn()
        const secondRelease = vi.fn()
        const second = deferred<any>()
        const acquire = vi.fn()
            .mockResolvedValueOnce({ release: firstRelease })
            .mockReturnValueOnce(second.promise)
        const lifetime = new DevToolConversationLease()

        await expect(lifetime.acquire({ conversationId: 'chat-a' } as any, acquire))
            .resolves.toBe(true)
        const replacing = lifetime.acquire({ conversationId: 'chat-b' } as any, acquire)

        expect(firstRelease).toHaveBeenCalledOnce()
        second.resolve({ release: secondRelease })
        await expect(replacing).resolves.toBe(true)
        lifetime.destroy()
        expect(secondRelease).toHaveBeenCalledOnce()
    })

    test('discards a late stale acquisition without detaching the current replacement', async () => {
        const first = deferred<any>()
        const second = deferred<any>()
        const firstRelease = vi.fn()
        const secondRelease = vi.fn()
        const acquire = vi.fn()
            .mockReturnValueOnce(first.promise)
            .mockReturnValueOnce(second.promise)
        const lifetime = new DevToolConversationLease()

        const older = lifetime.acquire({ conversationId: 'chat-a' } as any, acquire)
        const newer = lifetime.acquire({ conversationId: 'chat-b' } as any, acquire)
        second.resolve({ release: secondRelease })
        await expect(newer).resolves.toBe(true)
        first.resolve({ release: firstRelease })

        await expect(older).resolves.toBe(false)
        expect(firstRelease).toHaveBeenCalledOnce()
        expect(secondRelease).not.toHaveBeenCalled()
        lifetime.destroy()
        expect(secondRelease).toHaveBeenCalledOnce()
    })
})
