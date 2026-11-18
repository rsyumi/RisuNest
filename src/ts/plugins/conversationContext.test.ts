import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it } from 'vitest'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import type { PersistentRevisionLease } from '../storage/persistentDataStore'
import {
    normalizeConversationContextInput,
    readPinnedConversationContext,
    type ConversationContextInput,
} from './conversationContext'
import { conversationContextDatabase } from './conversationContext.testUtils'
import { authorNoteFingerprint } from '../authorNotePresets'

let sequence = 0

async function createStore() {
    const store = new IndexedDbPersistentDataStore(
        `conversation-context-${++sequence}`,
        new IDBFactory(),
        IDBKeyRange,
    )
    await store.open()
    const { revision } = await store.replaceFromDatabase(conversationContextDatabase())
    return { store, revision }
}

type Store = Awaited<ReturnType<typeof createStore>>['store']

function instrument(lease: PersistentRevisionLease, before?: Partial<Record<string, () => Promise<void>>>) {
    const calls: Array<{ method: string; args: unknown[] }> = []
    const reader = new Proxy(lease, {
        get(target, key) {
            const value = Reflect.get(target, key, target)
            if (typeof value !== 'function') return value
            return async (...args: unknown[]) => {
                calls.push({ method: String(key), args })
                const hook = before?.[String(key)]
                if (hook) {
                    delete before![String(key)]
                    await hook()
                }
                return value.apply(target, args)
            }
        },
    })
    return { reader, calls }
}

async function read(
    store: Store,
    input: ConversationContextInput,
    options: {
        selected?: { characterId: string; conversationId: string } | null
        allowPrivate?: boolean
        before?: Partial<Record<string, () => Promise<void>>>
    } = {},
) {
    const lease = await store.acquireRevision((await store.readRoot()).revision)
    const { reader, calls } = instrument(lease, options.before)
    try {
        const context = await readPinnedConversationContext(
            reader,
            normalizeConversationContextInput(input),
            { selected: options.selected ?? null, allowPrivate: options.allowPrivate ?? true },
        )
        return { context, calls, revision: lease.revision }
    } finally {
        await lease.release()
    }
}

const everything = {
    character: true,
    lore: true,
    persona: true,
    globals: true,
}

describe('conversation context read', () => {
    it('reads every requested part of a named conversation from one revision', async () => {
        const { store } = await createStore()
        const { context, revision } = await read(store, {
            characterId: 'char-plain',
            conversationId: 'conv-plain',
            include: everything,
            chatVariables: 'all',
            messages: { limit: 2, extraFields: ['__yumi_tr'] },
        })

        expect(context).toMatchObject({
            revision,
            characterId: 'char-plain',
            conversationId: 'conv-plain',
            characterIndex: 0,
            chatIndex: 0,
            selected: false,
            messageCount: 3,
        })
        expect(context!.character).toEqual({
            chaId: 'char-plain',
            name: 'Plain',
            nickname: 'Plainy',
            type: 'character',
            desc: 'Plain description',
            personality: 'Plain personality',
            scenario: 'Plain scenario',
            firstMessage: 'Hello from Plain',
            alternateGreetings: ['Plain greeting'],
            translatorNote: 'Plain translator note',
            defaultVariables: 'charvar=character\nboth=character',
            modules: ['module-char'],
        })
        expect(context!.conversation).toEqual({
            id: 'conv-plain',
            name: 'Chat conv-plain',
            note: 'conv-plain note',
            modules: ['module-chat'],
            bindedPersona: 'persona-bound',
        })
        expect(context!.lore!.globalLore!.map((entry) => entry.key)).toEqual(['char-plain-global'])
        expect(context!.lore!.loreSettings).toEqual({ tokenBudget: 100, scanDepth: 3, recursiveScanning: false })
        expect(context!.lore!.localLore!.map((entry) => entry.key)).toEqual(['conv-plain-local'])
        // The active preset's integration list wins over the stale stored root value, and the
        // bound persona adds its embedded module.
        expect(context!.lore!.modules.map((entry) => entry.id)).toEqual([
            'module-enabled', 'module-chat', 'module-char', 'module-int', 'module-ns', 'module-embedded',
        ])
        expect(context!.lore!.modules[0]).toEqual({
            id: 'module-enabled',
            name: 'Module module-enabled',
            lorebook: [expect.objectContaining({ key: 'module-enabled' })],
        })
        expect(context!.persona).toEqual({ id: 'persona-bound', name: 'Bound Persona', personaPrompt: 'bound prompt' })
        expect(context!.chatVariables).toEqual({ $present: 'stored', $both: 'stored-both', $number: 3, $flag: true })
        expect(context!.messages).toEqual({
            characterId: 'char-plain',
            conversationId: 'conv-plain',
            startIndex: 1,
            endIndex: 3,
            totalMessages: 3,
            hasMoreBefore: true,
            hasMoreAfter: false,
            messages: [
                { role: 'char', data: 'conv-plain second', chatId: 'conv-plain-m1', index: 1 },
                { role: 'user', data: 'conv-plain third', chatId: 'conv-plain-m2', __yumi_tr: { text: 'third' }, index: 2 },
            ],
        })
        expect(context!.globals).toEqual({
            username: 'Bound Persona',
            globalChatVariables: { shared: 'local', toggle_a: 'bound-a' },
            templateDefaultVariables: 'tmpl=template\nboth=template\ncharvar=template',
            customPromptTemplateToggle: 'toggle_a=Toggle A',
            presetRegex: [{ comment: 'preset regex', in: 'a', out: 'b', type: 'editoutput', ableFlag: false }],
            loreBookDepth: 7,
        })
    })

    it('keeps every part at the pinned revision while a commit lands during the read', async () => {
        const { store, revision } = await createStore()
        const metadata = (await store.readConversationMetadata('char-plain', 'conv-plain'))!.value
        const input: ConversationContextInput = {
            characterId: 'char-plain',
            conversationId: 'conv-plain',
            include: everything,
            chatVariables: ['$present'],
            messages: { limit: 1 },
        }
        const { context } = await read(store, input, {
            before: {
                readCharacter: async () => {
                    await store.commit({
                        expectedRevision: revision,
                        rootMutations: [
                            { type: 'set', key: 'loreBookDepth', value: 99 },
                            { type: 'set', key: 'explicitGlobalChatVariables', value: { shared: 'changed' } },
                        ],
                        conversations: [{
                            type: 'replace-range',
                            characterId: 'char-plain',
                            conversationId: 'conv-plain',
                            start: 3,
                            deleteCount: 0,
                            messages: [{ role: 'char', data: 'late reply', chatId: 'late' }],
                            conversation: {
                                ...metadata.conversation,
                                name: 'Renamed',
                                scriptstate: { $present: 'changed' },
                            },
                        }],
                    })
                },
            },
        })

        expect(context).toMatchObject({ revision, messageCount: 3, chatVariables: { $present: 'stored' } })
        expect(context!.conversation.name).toBe('Chat conv-plain')
        expect(context!.messages!.messages.map((message) => message.chatId)).toEqual(['conv-plain-m2'])
        expect(context!.globals).toMatchObject({
            loreBookDepth: 7,
            globalChatVariables: { shared: 'local', toggle_a: 'bound-a' },
        })

        const after = await read(store, input)
        expect(after.context).toMatchObject({
            revision: revision + 1,
            messageCount: 4,
            chatVariables: { $present: 'changed' },
            globals: { loreBookDepth: 99 },
        })
        expect(after.context!.conversation.name).toBe('Renamed')
        expect(after.context!.messages!.messages.map((message) => message.chatId)).toEqual(['late'])
    })

    it('reads the author note of a linked conversation from its preset even without other root parts', async () => {
        const { store, revision } = await createStore()
        const metadata = (await store.readConversationMetadata('char-plain', 'conv-plain'))!.value
        await store.commit({
            expectedRevision: revision,
            rootMutations: [{ type: 'set', key: 'authorNotePresets', value: [{ id: 'shared', name: 'Shared', text: 'Shared note' }] }],
            conversations: [{
                type: 'replace-range',
                characterId: 'char-plain',
                conversationId: 'conv-plain',
                start: 3,
                deleteCount: 0,
                messages: [],
                conversation: {
                    ...metadata.conversation,
                    note: 'stale copy',
                    authorNotePreset: { id: 'shared', fingerprint: authorNoteFingerprint('stale copy') },
                },
            }],
        })

        const { context } = await read(store, { characterId: 'char-plain', conversationId: 'conv-plain' })
        expect(context!.conversation.note).toBe('Shared note')
        expect(context!.conversation).not.toHaveProperty('authorNotePreset')
    })

    it('reads a missing target, or no selection without a target, as null', async () => {
        const { store } = await createStore()

        expect((await read(store, {})).context).toBeNull()
        expect((await read(store, { characterId: 'char-plain', conversationId: 'missing' })).context).toBeNull()
        expect((await read(store, { characterId: 'missing', conversationId: 'conv-plain' })).context).toBeNull()
        expect((await read(store, {}, { selected: { characterId: 'char-plain', conversationId: 'missing' } })).context).toBeNull()
    })

    it('omits only persona and globals without the db permission', async () => {
        const { store } = await createStore()
        const { context } = await read(store, {
            characterId: 'char-plain',
            conversationId: 'conv-plain',
            include: everything,
            chatVariables: ['$present'],
        }, { allowPrivate: false })

        expect(context).not.toHaveProperty('persona')
        expect(context).not.toHaveProperty('globals')
        expect(context!.character).toMatchObject({ chaId: 'char-plain' })
        expect(context!.lore!.modules.map((entry) => entry.id)).toContain('module-embedded')
        expect(context!.chatVariables).toEqual({ $present: 'stored' })
    })

    it('reads message rows only for a requested window and skips the root for the default parts', async () => {
        const { store } = await createStore()
        const target = { characterId: 'char-plain', conversationId: 'conv-plain' }

        const plain = await read(store, target)
        const plainMethods = plain.calls.map((call) => call.method)
        expect(plainMethods).not.toContain('readConversationWindow')
        expect(plainMethods).not.toContain('readConversation')
        expect(plainMethods).not.toContain('readRoot')
        expect(plainMethods).not.toContain('readPreset')
        expect(plain.context).not.toHaveProperty('messages')

        const windowed = await read(store, {
            ...target,
            include: everything,
            messages: { anchorMessageId: 'conv-plain-m1', before: 1, after: 0 },
        })
        expect(windowed.calls.filter((call) => call.method === 'readConversation')).toEqual([])
        expect(windowed.calls.filter((call) => call.method === 'readConversationWindow')).toEqual([{
            method: 'readConversationWindow',
            args: [{ ...target, anchorMessageId: 'conv-plain-m1', before: 1, after: 0 }],
        }])
        expect(windowed.context!.messages!.messages).toEqual([
            { role: 'user', data: 'conv-plain first', chatId: 'conv-plain-m0', index: 0 },
            { role: 'char', data: 'conv-plain second', chatId: 'conv-plain-m1', index: 1 },
        ])

        const absent = await read(store, { ...target, messages: { anchorMessageId: 'absent' } })
        expect(absent.context!.messages).toBeNull()
    })

    it('resolves a numeric preset selection and keeps root values without a preset', async () => {
        const { store, revision } = await createStore()
        const target = { characterId: 'char-plain', conversationId: 'conv-plain', include: { lore: true, globals: true } }
        await store.commit({ expectedRevision: revision, rootMutations: [{ type: 'set', key: 'botPresetsId', value: 1 }] })

        const numeric = await read(store, target)
        expect(numeric.context!.globals!.templateDefaultVariables).toBe('tmpl=template\nboth=template\ncharvar=template')
        expect(numeric.context!.lore!.modules.map((entry) => entry.id)).toContain('module-int')

        await store.commit({ expectedRevision: revision + 1, rootMutations: [{ type: 'set', key: 'botPresetsId', value: 'missing' }] })
        const missing = await read(store, target)
        expect(missing.context!.globals).toMatchObject({
            templateDefaultVariables: 'stale=root',
            customPromptTemplateToggle: 'stale',
            presetRegex: [],
        })
        expect(missing.context!.lore!.modules.map((entry) => entry.id)).toEqual([
            'module-enabled', 'module-chat', 'module-char', 'module-unused', 'module-embedded',
        ])
    })

    it('reads the conversation\'s bound preset and falls back to the selection for a binding that names no preset', async () => {
        const { store, revision } = await createStore()
        const target = { characterId: 'char-plain', conversationId: 'conv-plain', include: { lore: true, globals: true } }
        const bind = async (expectedRevision: number, promptPresetId: string) => {
            const metadata = (await store.readConversationMetadata('char-plain', 'conv-plain'))!.value
            await store.commit({
                expectedRevision,
                conversations: [{
                    type: 'replace-range', characterId: 'char-plain', conversationId: 'conv-plain',
                    start: 3, deleteCount: 0, messages: [],
                    conversation: { ...metadata.conversation, promptPresetId },
                }],
            })
        }

        await bind(revision, 'preset-other')
        const bound = await read(store, target)
        expect(bound.context!.globals!.templateDefaultVariables).toBe('')
        expect(bound.context!.lore!.modules.map((entry) => entry.id)).toContain('module-unused')
        expect(bound.context!.lore!.modules.map((entry) => entry.id)).not.toContain('module-int')

        await bind(revision + 1, 'missing')
        const missing = await read(store, target)
        expect(missing.context!.globals!.templateDefaultVariables).toBe('tmpl=template\nboth=template\ncharvar=template')
        expect(missing.context!.lore!.modules.map((entry) => entry.id)).toContain('module-int')
    })

    it.each([
        ['a non-object input', 'input', TypeError],
        ['one ID without the other', { characterId: 'char-plain' }, RangeError],
        ['an empty ID', { characterId: ' ', conversationId: 'conv-plain' }, RangeError],
        ['a non-boolean include', { include: { lore: 'yes' } }, TypeError],
        ['too many chat variable keys', { chatVariables: Array.from({ length: 257 }, (_, index) => `$${index}`) }, RangeError],
        ['a non-string chat variable key', { chatVariables: [1] }, TypeError],
        ['a host message field', { messages: { limit: 1, extraFields: ['data'] } }, RangeError],
        ['a prototype field', { messages: { limit: 1, extraFields: ['__proto__'] } }, RangeError],
        ['too many extra fields', { messages: { limit: 1, extraFields: Array.from({ length: 17 }, (_, index) => `__f${index}`) } }, RangeError],
        ['a range without limit', { messages: { startIndex: 0 } }, RangeError],
        ['an oversized anchor window', { messages: { anchorMessageId: 'a', before: 100, after: 100 } }, RangeError],
    ])('rejects %s before reading', (_name, input, error) => {
        expect(() => normalizeConversationContextInput(input)).toThrow(error)
    })
})
