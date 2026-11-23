import { describe, expect, it, vi } from 'vitest'
import type { ConversationContext } from '../conversationContext'
import type { ConversationPatchResult } from '../conversationPatch'
import { createRisunestPrivateApi } from './risunestPrivateApi'

function setup(granted: boolean | Promise<boolean> = true) {
    const context = { revision: 1 } as ConversationContext
    const readConversationContext = vi.fn(async () => context)
    const hasDatabasePermission = vi.fn(async () => granted)
    const hasDatabaseWritePermission = vi.fn(async () => granted)
    const hasHostToolPermission = vi.fn(async () => granted)
    const lifetime = new AbortController()
    const patchConversation = vi.fn(async (_request: unknown, _signal?: AbortSignal): Promise<ConversationPatchResult> => ({ status: 'applied', revision: 2 }))
    const listTools = vi.fn(async () => ({ scope: 'scope', tools: [] }))
    const callTool = vi.fn(async (_request: unknown, _signal?: AbortSignal) => [{ type: 'text' as const, text: 'done' }])
    const chatView = { register: vi.fn(() => ({ id: 'view-1' })), unregister: vi.fn(), dispose: vi.fn() }
    const generationEnd = { register: vi.fn(() => ({ id: 'end-1' })), unregister: vi.fn(), dispose: vi.fn() }
    const api = createRisunestPrivateApi({
        databaseAccess: { readConversationContext },
        patchAccess: { patchConversation },
        hostTools: { listTools, callTool },
        chatView,
        generationEnd,
        hasDatabasePermission,
        hasDatabaseWritePermission,
        hasHostToolPermission,
        lifetimeSignal: lifetime.signal,
    })
    return { api, context, readConversationContext, hasDatabasePermission, hasDatabaseWritePermission, hasHostToolPermission, lifetime, patchConversation, listTools, callTool, chatView, generationEnd }
}

describe('risunestReadConversationContext', () => {
    it('reads parts that need no permission without asking for one', async () => {
        const { api, context, readConversationContext, hasDatabasePermission } = setup()

        await expect(api.readConversationContext({
            characterId: 'char',
            conversationId: 'conv',
            include: { character: true, lore: true },
            chatVariables: ['$key'],
            messages: { limit: 4, extraFields: ['__plugin'] },
        })).resolves.toBe(context)

        expect(hasDatabasePermission).not.toHaveBeenCalled()
        expect(readConversationContext).toHaveBeenCalledWith({
            target: { characterId: 'char', conversationId: 'conv' },
            include: { character: true, lore: true, persona: false, globals: false },
            chatVariables: ['$key'],
            messages: { window: { limit: 4 }, extraFields: ['__plugin'] },
        }, { allowPrivate: false, signal: expect.any(AbortSignal) })
    })

    it.each([
        ['persona', true],
        ['persona', false],
        ['globals', true],
        ['globals', false],
    ])('passes the db permission decision for %s (granted: %s)', async (part, granted) => {
        const { api, readConversationContext, hasDatabasePermission } = setup(granted)

        await api.readConversationContext({ include: { [part]: true } })

        expect(hasDatabasePermission).toHaveBeenCalledTimes(1)
        expect(readConversationContext).toHaveBeenCalledWith(
            expect.objectContaining({ target: null }),
            expect.objectContaining({ allowPrivate: granted }),
        )
    })

    it.each([
        ['an invalid part', { include: { globals: true }, chatVariables: 5 }],
        ['a signal that is not an AbortSignal', { include: { persona: true }, signal: 'stop' }],
    ])('rejects %s before asking or reading', async (_name, input) => {
        const { api, readConversationContext, hasDatabasePermission } = setup()

        await expect(api.readConversationContext(input)).rejects.toThrow(TypeError)

        expect(hasDatabasePermission).not.toHaveBeenCalled()
        expect(readConversationContext).not.toHaveBeenCalled()
    })

    it('stops for an aborted caller signal or a plugin unload during the permission prompt', async () => {
        const aborted = setup()
        const controller = new AbortController()
        controller.abort()
        await expect(aborted.api.readConversationContext({ signal: controller.signal }))
            .rejects.toMatchObject({ name: 'AbortError' })
        expect(aborted.readConversationContext).not.toHaveBeenCalled()

        let answer!: (granted: boolean) => void
        const unloading = setup(new Promise<boolean>((resolve) => { answer = resolve }))
        const pending = unloading.api.readConversationContext({ include: { globals: true } })
        await vi.waitFor(() => expect(unloading.hasDatabasePermission).toHaveBeenCalled())
        unloading.lifetime.abort()
        answer(true)
        await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        expect(unloading.readConversationContext).not.toHaveBeenCalled()
    })
})

describe('risunestPatchConversation', () => {
    const input = {
        characterId: 'char',
        conversationId: 'conv',
        mutationId: 'patch-1',
        messages: [{ index: 0, messageId: 'm0', set: { __tr: 'text' } }],
    }

    it('validates before patching, passes the request on and replays the outcome', async () => {
        const { api, patchConversation } = setup()

        await expect(api.patchConversation({ ...input, messages: [{ index: 0, messageId: 'm0', set: { role: 'user' } }] }))
            .rejects.toThrow(TypeError)
        expect(patchConversation).not.toHaveBeenCalled()

        await expect(api.patchConversation(input)).resolves.toEqual({ status: 'applied', revision: 2 })
        await expect(api.patchConversation(input)).resolves.toEqual({ status: 'already-applied', revision: 2 })
        expect(patchConversation).toHaveBeenCalledOnce()
        expect(patchConversation.mock.calls[0]).toEqual([
            { characterId: 'char', conversationId: 'conv', mutationId: 'patch-1', messages: input.messages, chatVariables: [] },
            expect.any(AbortSignal),
        ])
    })

    it('rejects a patch the user declined dbWrite for, without patching or recording the mutation ID', async () => {
        const { api, patchConversation, hasDatabaseWritePermission } = setup()
        hasDatabaseWritePermission.mockResolvedValueOnce(false)

        await expect(api.patchConversation(input)).rejects.toThrow('Conversation patches require the dbWrite permission')
        expect(patchConversation).not.toHaveBeenCalled()
        await expect(api.patchConversation(input)).resolves.toEqual({ status: 'applied', revision: 2 })
    })

    it('stops for an aborted signal without recording the mutation ID', async () => {
        const { api, patchConversation, lifetime } = setup()
        const controller = new AbortController()
        controller.abort()

        await expect(api.patchConversation({ ...input, signal: controller.signal })).rejects.toMatchObject({ name: 'AbortError' })
        expect(patchConversation).not.toHaveBeenCalled()
        await expect(api.patchConversation(input)).resolves.toMatchObject({ status: 'applied' })
        await expect(api.patchConversation({ ...input, mutationId: 'patch-2', signal: 'stop' })).rejects.toThrow(TypeError)

        let finish!: () => void
        patchConversation.mockImplementationOnce((_request, signal) => new Promise((resolve, reject) => {
            finish = () => signal?.aborted ? reject(signal.reason) : resolve({ status: 'applied', revision: 3 })
        }))
        const pending = api.patchConversation({ ...input, mutationId: 'patch-3' })
        await vi.waitFor(() => expect(patchConversation).toHaveBeenCalledTimes(2))
        lifetime.abort()
        finish()
        await expect(pending).rejects.toMatchObject({ name: 'AbortError' })

        const unloading = setup()
        unloading.hasDatabaseWritePermission.mockImplementationOnce(async () => {
            unloading.lifetime.abort()
            return false
        })
        await expect(unloading.api.patchConversation(input)).rejects.toMatchObject({ name: 'AbortError' })
    })
})

describe('risunest host tools', () => {
    const input = { scope: 'scope', source: 'internal:dice', name: 'roll', arguments: { sides: 6 } }

    it('asks for the hostTools permission alone and passes the call on with a signal', async () => {
        const { api, hasDatabasePermission, hasHostToolPermission, listTools, callTool } = setup()

        await expect(api.listHostTools()).resolves.toEqual({ scope: 'scope', tools: [] })
        await expect(api.callHostTool(input)).resolves.toEqual([{ type: 'text', text: 'done' }])
        expect(hasHostToolPermission).toHaveBeenCalledTimes(2)
        expect(hasDatabasePermission).not.toHaveBeenCalled()
        expect(listTools).toHaveBeenCalledTimes(1)
        expect(callTool).toHaveBeenCalledWith(input, expect.any(AbortSignal))
        await expect(api.callHostTool({ ...input, source: '' })).rejects.toThrow(TypeError)
        await expect(api.callHostTool({ ...input, signal: 'stop' })).rejects.toThrow(TypeError)
    })

    it('rejects both methods when the hostTools permission is denied', async () => {
        const { api, listTools, callTool } = setup(false)

        await expect(api.listHostTools()).rejects.toThrow(/hostTools permission/)
        await expect(api.callHostTool(input)).rejects.toThrow(/hostTools permission/)
        expect(listTools).not.toHaveBeenCalled()
        expect(callTool).not.toHaveBeenCalled()
    })

    it('stops for an aborted caller signal or a plugin unload', async () => {
        const { api, lifetime, callTool } = setup()
        const controller = new AbortController()
        controller.abort()

        await expect(api.callHostTool({ ...input, signal: controller.signal })).rejects.toMatchObject({ name: 'AbortError' })
        expect(callTool).not.toHaveBeenCalled()
        let seen: AbortSignal | undefined
        callTool.mockImplementationOnce(async (_request, signal) => {
            seen = signal
            lifetime.abort()
            return []
        })
        await api.callHostTool(input)
        expect(seen?.aborted).toBe(true)
        await expect(api.listHostTools()).rejects.toMatchObject({ name: 'AbortError' })
    })
})

describe('risunestOnChatView', () => {
    it('registers without asking for a permission and unregisters through unregisterUIPart', () => {
        const { api, chatView, hasDatabasePermission } = setup()
        const callback = vi.fn()

        expect(api.onChatView(callback)).toEqual({ id: 'view-1' })
        expect(chatView.register).toHaveBeenCalledWith(callback)
        expect(hasDatabasePermission).not.toHaveBeenCalled()
        api.unregisterUIPart('view-1')
        expect(chatView.unregister).toHaveBeenCalledWith('view-1')
        api.unregisterUIPart(1)
        expect(chatView.unregister).toHaveBeenCalledTimes(1)
        expect(() => api.onChatView('not a function')).toThrow(TypeError)
    })

    it("drops the plugin's listeners on unload and refuses new ones", () => {
        const { api, chatView, lifetime } = setup()
        lifetime.abort()

        expect(chatView.dispose).toHaveBeenCalledTimes(1)
        expect(() => api.onChatView(vi.fn())).toThrow(expect.objectContaining({ name: 'AbortError' }))
        expect(chatView.register).not.toHaveBeenCalled()
    })
})

describe('risunestOnGenerationEnd', () => {
    it('registers without asking for a permission and unregisters through unregisterUIPart', () => {
        const { api, chatView, generationEnd, hasDatabasePermission } = setup()
        const callback = vi.fn()

        expect(api.onGenerationEnd(callback)).toEqual({ id: 'end-1' })
        expect(generationEnd.register).toHaveBeenCalledWith(callback)
        expect(hasDatabasePermission).not.toHaveBeenCalled()
        api.unregisterUIPart('end-1')
        expect(generationEnd.unregister).toHaveBeenCalledWith('end-1')
        expect(chatView.unregister).toHaveBeenCalledWith('end-1')
        expect(() => api.onGenerationEnd('not a function')).toThrow(TypeError)
    })

    it("drops the plugin's listeners on unload and refuses new ones", () => {
        const { api, generationEnd, lifetime } = setup()
        lifetime.abort()

        expect(generationEnd.dispose).toHaveBeenCalledTimes(1)
        expect(() => api.onGenerationEnd(vi.fn())).toThrow(expect.objectContaining({ name: 'AbortError' }))
        expect(generationEnd.register).not.toHaveBeenCalled()
    })
})
