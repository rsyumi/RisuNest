import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'

// The prompt builder's own module, persona and variable functions run against the selection
// state below; the app's stores and card code are replaced because their import graph starts
// UI effects that need a running app.
vi.mock('../stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: {} },
        selectedCharID: writable(-1),
        HideIconStore: writable(false),
        moduleBackgroundEmbedding: writable(''),
        ReloadGUIPointer: writable(0),
    }
})
vi.mock('../storage/database.svelte', async () => {
    const { get } = await import('svelte/store')
    const { DBState, selectedCharID } = await import('../stores.svelte')
    const getDatabase = () => DBState.db
    const getCurrentCharacter = () => DBState.db.characters?.[get(selectedCharID)]
    return {
        getDatabase,
        getCurrentCharacter,
        getCurrentChat: () => {
            const character = getCurrentCharacter()
            return character?.chats?.[character.chatPage]
        },
    }
})
vi.mock('../characters', () => ({}))
vi.mock('../characterCards', () => ({}))
vi.mock('../globalApi.svelte', () => ({}))
vi.mock('../alert', () => ({}))
vi.mock('../process/lorebook.svelte', () => ({}))
vi.mock('../interchangeability', () => ({}))
vi.mock('../rpack/rpack_js', () => ({}))
vi.mock('src/lib/UI/PopupList.svelte', () => ({ default: {} }))

import { DBState, selectedCharID } from '../stores.svelte'
import { getModules } from '../process/modules'
import { getPersonaPrompt, getUserName, parseKeyValue } from '../util'
import { getChatVarFromConversation, getGlobalChatVar } from '../parser/chatVar.svelte'
import { deriveEffectiveToggleVariables } from '../storage/effectiveIdentityState'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import {
    normalizeConversationContextInput,
    readPinnedConversationContext,
    type ConversationContext,
} from './conversationContext'
import {
    conversationContextDatabase,
    conversationContextHostDatabase,
} from './conversationContext.testUtils'

async function readContext(characterId: string, conversationId: string) {
    const store = new IndexedDbPersistentDataStore(
        `conversation-context-host-${characterId}-${conversationId}`,
        new IDBFactory(),
        IDBKeyRange,
    )
    await store.open()
    const { revision } = await store.replaceFromDatabase(conversationContextDatabase())
    const lease = await store.acquireRevision(revision)
    try {
        return (await readPinnedConversationContext(lease, normalizeConversationContextInput({
            characterId,
            conversationId,
            include: { character: true, lore: true, persona: true, globals: true },
            chatVariables: 'all',
        }), { selected: null, allowPrivate: true }))!
    } finally {
        await lease.release()
    }
}

function selectHostConversation(characterId: string, conversationId: string) {
    const database = conversationContextHostDatabase()
    const characterIndex = database.characters.findIndex((entry) => entry.chaId === characterId)
    const character = database.characters[characterIndex]
    character.chatPage = character.chats.findIndex((entry) => entry.id === conversationId)
    const chat = character.chats[character.chatPage]
    DBState.db = database
    deriveEffectiveToggleVariables(database, chat)
    selectedCharID.set(characterIndex)
    return { database, character, chat }
}

/** What a plugin does with the context: the scriptstate value, then the character default, then the template default. */
function pluginChatVar(context: ConversationContext, key: string): string {
    const stored = context.chatVariables?.['$' + key]
    if (stored !== undefined && stored !== null) return stored.toString()
    const defaults = parseKeyValue(context.character!.defaultVariables as string)
        .concat(parseKeyValue(context.globals!.templateDefaultVariables))
    return defaults.find(([name]) => name === key)?.[1] ?? 'null'
}

describe.each([
    ['a character chat with a bound persona and toggle binding', 'char-plain', 'conv-plain'],
    ['a character chat with the selected persona', 'char-plain', 'conv-second'],
])('conversation context for %s', (_name, characterId, conversationId) => {
    it('matches the module set, persona and variables the prompt builder uses', async () => {
        const context = await readContext(characterId, conversationId)
        const { database, character, chat } = selectHostConversation(characterId, conversationId)

        expect(context.lore!.modules.map((entry) => entry.id)).toEqual(getModules().map((entry) => entry.id))
        expect(context.lore!.modules.map((entry) => entry.lorebook)).toEqual(getModules().map((entry) => entry.lorebook))
        expect(context.persona!.name).toBe(getUserName())
        expect(context.persona!.personaPrompt).toBe(getPersonaPrompt())
        expect(context.globals!.username).toBe(getUserName())

        for (const key of ['present', 'both', 'number', 'flag', 'charvar', 'tmpl', 'groupvar', 'absent']) {
            expect(pluginChatVar(context, key)).toBe(getChatVarFromConversation(database, character.chaId, chat, key))
        }
        const globalKeys = new Set([
            ...Object.keys(database.globalChatVariables),
            ...Object.keys(chat.GLGlobalVariables ?? {}),
            ...Object.keys(context.globals!.globalChatVariables),
            'absent',
        ])
        for (const key of globalKeys) {
            expect(context.globals!.globalChatVariables[key] ?? 'null').toBe(getGlobalChatVar(key))
        }
    })
})
