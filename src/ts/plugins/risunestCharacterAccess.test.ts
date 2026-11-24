import { afterEach, describe, expect, it, vi } from 'vitest'
import type { character } from '../storage/database.svelte'
import type { PersistentDataStore } from '../storage/persistentDataStore'
import { openProductionLibrary } from '../storage/tests/productionLibraryFixture'
import type { PluginDatabaseAccess } from './pluginDatabaseAccess'

const state = vi.hoisted(() => ({
    store: null as PersistentDataStore | null,
    writeGranted: true,
    writeAsked: 0,
}))

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
vi.mock('../gui/receivedDisplaySettings', () => ({ applyReceivedDisplaySettings: vi.fn(async () => undefined) }))
vi.mock('../storage/persistentDataStoreFactory', async (importOriginal) => ({
    ...await importOriginal<typeof import('../storage/persistentDataStoreFactory')>(),
    getPersistentDataStore: () => state.store,
    getRawPersistentDataStore: () => state.store,
}))

afterEach(() => {
    state.writeGranted = true
    state.writeAsked = 0
})

/// The fixture library with Beta and Alpha in the sidebar and Gamma in the trash, nothing open.
async function openLibrary(name: string) {
    const library = await openProductionLibrary(name, (store) => {
        state.store = store
    }, (initial) => {
        initial.characters.find((value) => value.chaId === 'char-c')!.trashTime = Date.now()
        initial.characterOrder = ['char-b', 'char-a']
    })
    const { get } = await import('svelte/store')
    const characters = await import('../characters')
    const { doingChat } = await import('../process/generationState')
    const { createCatalogCharacterStub } = await import('../storage/workingSetCatalog')
    const { flushPendingData } = await import('../storage/persistentDataRuntime.svelte')
    const { createRisunestCharacterAccess } = await import('./risunestCharacterAccess')
    const live = () => library.database.getDatabase()
    const access = createRisunestCharacterAccess({
        pluginName: 'synthetic-plugin',
        databaseAccess: {} as PluginDatabaseAccess,
        hasDatabasePermission: async () => true,
        hasDatabaseWritePermission: async () => {
            state.writeAsked += 1
            return state.writeGranted
        },
        lifetimeSignal: new AbortController().signal,
    })
    return {
        ...library,
        access,
        characters,
        doingChat,
        createCatalogCharacterStub,
        live,
        /** Opening a character leaves its activation edits unsaved. */
        async open(id: string) {
            expect(await characters.changeChar(live().characters.findIndex((value) => value.chaId === id))).toBe(true)
        },
        settle: () => flushPendingData('test-settle'),
        openId: () => live().characters[get(library.stores.selectedCharID)]?.chaId ?? null,
        resident: (id: string) => live().characters.find((value) => value.chaId === id),
        revision: async () => (await library.store.readRoot()).revision,
        storedOrder: async () => (await library.store.readRoot()).value.characterOrder,
    }
}

describe('plugin character access', { timeout: 30_000 }, () => {
    it('creates a blank character with the given fields last in the sidebar without opening it', async () => {
        const library = await openLibrary('risunest-character-create')
        await library.open('char-b')

        const result = await library.access.createCharacter({
            fields: { name: 'Synthetic', desc: 'Synthetic description', firstMessage: 'Synthetic greeting' },
        })

        expect(result).toEqual({ status: 'applied', id: expect.any(String) })
        const id = (result as { id: string }).id
        const blank: Partial<character> = library.characters.createBlankChar()
        delete blank.chaId
        delete blank.chats
        expect((await library.store.readCharacter(id))?.value).toMatchObject({
            ...blank,
            name: 'Synthetic',
            desc: 'Synthetic description',
            firstMessage: 'Synthetic greeting',
        })
        const conversations = await library.store.queryConversations({ characterId: id, order: 'configured', limit: 10 })
        expect(conversations.items).toHaveLength(1)
        expect(await library.storedOrder()).toEqual(['char-b', 'char-a', id])
        expect(library.live().characterOrder).toEqual(['char-b', 'char-a', id])
        expect(library.openId()).toBe('char-b')
    })

    it('rejects a bad argument, a host-owned field or a field the validator refuses before asking, and stores nothing', async () => {
        const library = await openLibrary('risunest-character-create-rejected')
        const revision = await library.revision()

        for (const input of [undefined, { name: 'Synthetic' }, { fields: ['Synthetic'] }]) {
            await expect(library.access.createCharacter(input)).rejects.toThrow(TypeError)
        }
        await expect(library.access.trashCharacter({ id: '' })).rejects.toThrow(TypeError)
        await expect(library.access.restoreCharacter('char-c')).rejects.toThrow(TypeError)
        for (const key of ['type', 'chaId', 'trashTime']) {
            expect(await library.access.createCharacter({ fields: { name: 'Synthetic', [key]: key === 'trashTime' ? 1 : 'group' } }))
                .toEqual({ status: 'rejected', reason: `${key} cannot be set` })
        }
        expect(await library.access.createCharacter({
            fields: { chats: [{ id: 'same', message: [] }, { id: 'same', message: [] }] },
        })).toEqual({ status: 'rejected', reason: 'Plugin character contains duplicate conversation ID same' })

        expect(state.writeAsked).toBe(0)
        expect(await library.revision()).toBe(revision)
        expect(library.live().characters.map((value) => value.chaId)).toEqual(['char-b', 'char-a', 'char-c'])
    })

    it('answers denied and stores nothing when the write permission is declined', async () => {
        const library = await openLibrary('risunest-character-denied')
        const revision = await library.revision()
        state.writeGranted = false

        expect(await library.access.createCharacter({ fields: { name: 'Synthetic' } })).toEqual({ status: 'denied' })
        expect(await library.access.trashCharacter({ id: 'char-a' })).toEqual({ status: 'denied' })
        expect(await library.access.restoreCharacter({ id: 'char-c' })).toEqual({ status: 'denied' })

        expect(await library.revision()).toBe(revision)
        expect(library.live().characters.map((value) => value.chaId)).toEqual(['char-b', 'char-a', 'char-c'])
        expect(library.live().characterOrder).toEqual(['char-b', 'char-a'])
    })

    it('closes the open character it moves to the trash, then restores it last in the sidebar with its chats', async () => {
        const library = await openLibrary('risunest-character-trash-open')
        await library.open('char-b')

        expect(await library.access.trashCharacter({ id: 'char-b' })).toEqual({ status: 'applied' })

        expect(library.openId()).toBeNull()
        expect(library.live().characterOrder).toEqual(['char-a'])
        expect(await library.storedOrder()).toEqual(['char-a'])
        const trashTime = (await library.store.readCharacter('char-b'))!.value.trashTime
        expect(trashTime).toEqual(expect.any(Number))
        // The trash list shows the characters whose working copy carries a trash time.
        expect(library.resident('char-b')!.trashTime).toBe(trashTime)

        expect(await library.access.restoreCharacter({ id: 'char-b' })).toEqual({ status: 'applied' })

        expect(library.live().characterOrder).toEqual(['char-a', 'char-b'])
        expect(await library.storedOrder()).toEqual(['char-a', 'char-b'])
        expect(library.resident('char-b')).not.toHaveProperty('trashTime')
        expect((await library.store.readCharacter('char-b'))!.value).not.toHaveProperty('trashTime')
        expect((await library.store.readConversation('char-b', 'conv-beta'))!.value.message).toHaveLength(3)
    })

    it('moves a character that is not open to the trash without closing the open one, and finds no unknown ID', async () => {
        const library = await openLibrary('risunest-character-trash-other')
        await library.open('char-b')

        expect(await library.access.trashCharacter({ id: 'char-a' })).toEqual({ status: 'applied' })

        expect(library.openId()).toBe('char-b')
        expect(library.live().characterOrder).toEqual(['char-b'])
        expect(await library.storedOrder()).toEqual(['char-b'])
        expect(library.resident('char-a')!.trashTime).toEqual(expect.any(Number))
        expect((await library.store.readCharacter('char-a'))!.value.trashTime).toBe(library.resident('char-a')!.trashTime)

        const revision = await library.revision()
        expect(await library.access.trashCharacter({ id: 'missing' })).toEqual({ status: 'not-found' })
        expect(await library.access.restoreCharacter({ id: 'missing' })).toEqual({ status: 'not-found' })
        expect(await library.revision()).toBe(revision)
    })

    it('changes nothing when trashing a trashed character or restoring one that is not in the trash', async () => {
        const library = await openLibrary('risunest-character-noop')
        // The trash list opens a trashed character, which stays open.
        await library.open('char-c')
        await library.settle()
        const trashTime = library.resident('char-c')!.trashTime
        const revision = await library.revision()

        expect(await library.access.trashCharacter({ id: 'char-c' })).toEqual({ status: 'applied' })
        expect(await library.access.restoreCharacter({ id: 'char-a' })).toEqual({ status: 'applied' })

        expect(await library.revision()).toBe(revision)
        expect(library.openId()).toBe('char-c')
        expect(library.resident('char-c')!.trashTime).toBe(trashTime)
        expect(library.live().characterOrder).toEqual(['char-b', 'char-a'])
    })

    it('keeps the open character and stores nothing while a response is generated', async () => {
        const library = await openLibrary('risunest-character-trash-busy')
        await library.open('char-b')
        const revision = await library.revision()
        library.doingChat.set(true)
        try {
            expect(await library.access.trashCharacter({ id: 'char-b' })).toEqual({ status: 'rejected', reason: 'busy' })
        } finally {
            library.doingChat.set(false)
        }

        expect(library.openId()).toBe('char-b')
        expect(await library.revision()).toBe(revision)
        expect(library.live().characterOrder).toEqual(['char-b', 'char-a'])
    })

    it('does not move an archived character to the trash', async () => {
        const library = await openLibrary('risunest-character-archived')
        // Only the native store archives, so the working copy holds what it lists for an archived character.
        const characters = library.live().characters
        characters[characters.findIndex((value) => value.chaId === 'char-a')] = library.createCatalogCharacterStub({
            id: 'char-a', name: 'Alpha', configuredIndex: 1, recentAt: 300, trashed: false, conversationCount: 0,
            type: 'character', archived: { archivedAt: 1, conversationCount: 2, messageCount: 132 },
        })
        const revision = await library.revision()

        expect(await library.access.trashCharacter({ id: 'char-a' })).toEqual({ status: 'rejected', reason: 'archived' })
        expect(await library.access.restoreCharacter({ id: 'char-a' })).toEqual({ status: 'applied' })

        expect(await library.revision()).toBe(revision)
        expect(library.live().characterOrder).toEqual(['char-b', 'char-a'])
    })
})
