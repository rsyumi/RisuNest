import { afterEach, expect, it, vi } from 'vitest'
import { bootChatInteractionApp } from '../lib/ChatScreens/chatInteractionApp.testSupport'

let app: Awaited<ReturnType<typeof bootChatInteractionApp>> | undefined
let cleanup: (() => void)[] = []

afterEach(() => {
    for (const step of cleanup.splice(0).reverse()) step()
    app?.restore()
    app = undefined
    vi.restoreAllMocks()
})

it('refuses a chat scope edit when a generation starts while it waits behind another edit', async () => {
    const current = app = await bootChatInteractionApp()
    const lease = await current.runtime.acquireCompleteConversation('module-scope-generation')
    cleanup.push(() => lease.release())
    const { setModuleScope } = await import('./moduleScopes')
    const { doingChat } = await import('./process/generationState')
    const { captureGenerationConversationOperation } = await import('./process/generationConversationOperation')

    // Hold the character edit's commit so the chat edit queues behind it.
    let release!: () => void
    let entered!: () => void
    const held = new Promise<void>((resolve) => { release = resolve })
    const started = new Promise<void>((resolve) => { entered = resolve })
    const commit = current.runtime.store.commit.bind(current.runtime.store)
    vi.spyOn(current.runtime.store, 'commit').mockImplementationOnce(async (...args) => {
        entered()
        await held
        return commit(...args)
    })
    const characterEdit = setModuleScope('character', 'synthetic-first', true)
    await started
    const chatEdit = setModuleScope('chat', 'synthetic-queued', true)

    doingChat.set(true)
    cleanup.push(() => doingChat.set(false))
    const operation = captureGenerationConversationOperation({
        session: current.runtime.getActiveConversationSession(),
        getCurrentSession: () => current.runtime.getActiveConversationSession(),
        chat: current.selected().chats[0],
        getCurrentChat: () => current.selected().chats[0],
        continueLast: true,
    })
    cleanup.push(() => operation.release())
    release()

    expect(await characterEdit).toBe(true)
    expect(await chatEdit).toBe(false)
    expect(current.selected().chats[0].modules).toBeUndefined()
    // The generation still records its response.
    expect(operation.commitData('Synthetic generated response')).toBe(true)
}, 180_000)
