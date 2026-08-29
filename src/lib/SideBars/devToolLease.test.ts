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
})
