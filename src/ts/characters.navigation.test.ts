import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

const mocks = vi.hoisted(() => ({
    database: { characters: [] as any[] },
    nextId: 0,
    activateCharacter: vi.fn(async (_id?: string, _options?: any) => true),
    commitCharacterAddition: vi.fn(async (request: any, _reason: string) => request.install()),
    markPersistentDataDirty: vi.fn(),
    replacePersistentDatabase: vi.fn(async (_database: any, _reason: string) => undefined),
    getColdStorageItem: vi.fn(),
    alertConfirm: vi.fn(async () => true),
    alertAddCharacter: vi.fn(async () => 'createfromScratch'),
    alertError: vi.fn(),
    changeChatTo: vi.fn(async (_idOrIndex?: string | number) => true),
    findCharacterbyId: vi.fn(),
}))

vi.mock('uuid', () => ({
    v4: () => `generated-${++mocks.nextId}`,
}))
vi.mock('./storage/database.svelte', () => ({
    defaultSdDataFunc: () => ({}),
    getDatabase: (options?: { snapshot?: boolean }) => options?.snapshot
        ? structuredClone(mocks.database)
        : mocks.database,
    getCharacterByIndex: (index: number) => mocks.database.characters[index],
    setCharacterByIndex: (index: number, character: unknown) => {
        mocks.database.characters[index] = character
    },
}))
vi.mock('./alert', async () => {
    const { writable } = await import('svelte/store')
    return {
        alertAddCharacter: mocks.alertAddCharacter,
        alertConfirm: mocks.alertConfirm,
        alertError: mocks.alertError,
        alertNormal: vi.fn(),
        alertSelect: vi.fn(),
        alertStore: writable({ type: 'none', msg: '' }),
        alertWait: vi.fn(),
    }
})
vi.mock('../lang', () => ({ language: { errors: {} } }))
vi.mock('./util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    findCharacterbyId: mocks.findCharacterbyId,
    findCharacterIndexbyId: vi.fn(),
    getUserName: vi.fn(),
    selectMultipleFile: vi.fn(),
    selectSingleFile: vi.fn(),
}))
vi.mock('./media', () => ({ getImageType: vi.fn() }))
vi.mock('./stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: mocks.database },
        MobileGUIStack: writable(0),
        OpenRealmStore: writable(false),
        selectedCharID: writable(-1),
    }
})
vi.mock('./globalApi.svelte', () => ({
    AppendableBuffer: class {},
    changeChatTo: mocks.changeChatTo,
    checkCharOrder: vi.fn(),
    downloadFile: vi.fn(),
    getFileSrc: vi.fn(),
}))
vi.mock('./process/inlayScreen', () => ({ updateInlayScreen: (value: unknown) => value }))
vi.mock('./parser/parser.svelte', () => ({ parseMarkdownSafe: (value: string) => value }))
vi.mock('./translator/translator', () => ({ translateHTML: vi.fn() }))
vi.mock('./process/index.svelte', async () => {
    const { writable } = await import('svelte/store')
    return { doingChat: writable(false) }
})
vi.mock('./characterCards', () => ({ importCharacter: vi.fn() }))
vi.mock('./pngChunk', () => ({ PngChunk: {} }))
vi.mock('./process/coldstorage.svelte', () => ({ getColdStorageItem: mocks.getColdStorageItem }))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    activateCharacter: mocks.activateCharacter,
    commitCharacterAddition: mocks.commitCharacterAddition,
    markPersistentDataDirty: mocks.markPersistentDataDirty,
    replacePersistentDatabase: mocks.replacePersistentDatabase,
}))

import {
    addCharacter,
    addNewChat,
    changeChar,
    characterFormatUpdate,
    createBlankChar,
    createNewCharacter,
    createNewGroup,
    removeChar,
    removeChat,
} from './characters'
import { MobileGUIStack } from './stores.svelte'

describe('runtime chat identity', () => {
    beforeEach(() => {
        mocks.database.characters = []
        mocks.nextId = 0
        vi.clearAllMocks()
        mocks.activateCharacter.mockResolvedValue(true)
        mocks.alertConfirm.mockResolvedValue(true)
        mocks.alertAddCharacter.mockResolvedValue('createfromScratch')
        mocks.commitCharacterAddition.mockImplementation(async (request) => request.install())
        mocks.replacePersistentDatabase.mockImplementation(async (database) => {
            Object.assign(mocks.database, structuredClone(database))
        })
    })

    it('navigates by the character ID captured before hydration', async () => {
        const first = createBlankChar()
        const second = createBlankChar()
        mocks.database.characters.push(first, second)
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.database.characters.reverse()
            return true
        })

        const changed = await changeChar(0)

        expect(changed).toBe(true)
        expect(mocks.activateCharacter).toHaveBeenCalledWith(first.chaId, undefined)
    })

    it('replaces persistent data before publishing character removal', async () => {
        const first = createBlankChar()
        const second = createBlankChar()
        mocks.database.characters.push(first, second)

        await removeChar(first.chaId, first.name, 'permanentForce')

        expect(mocks.replacePersistentDatabase).toHaveBeenCalledOnce()
        const [candidate, reason] = mocks.replacePersistentDatabase.mock.calls[0]
        expect(reason).toBe('character-removal')
        expect(candidate.characters.map((character: any) => character.chaId)).toEqual([second.chaId])
    })

    it('removes the character ID captured before confirmation', async () => {
        const first = createBlankChar()
        const second = createBlankChar()
        mocks.database.characters.push(first, second)
        mocks.alertConfirm.mockImplementationOnce(async () => {
            mocks.database.characters.reverse()
            return true
        }).mockResolvedValueOnce(true)

        await removeChar(0, first.name, 'permanent')

        const [candidate] = mocks.replacePersistentDatabase.mock.calls[0]
        expect(candidate.characters.map((character: any) => character.chaId)).toEqual([second.chaId])
    })

    it('persists a restored cold character before activating it', async () => {
        const stub = createBlankChar()
        stub.coldstorage = 'cold-key'
        const restored = structuredClone(stub)
        restored.name = 'Restored'
        delete restored.coldstorage
        mocks.database.characters.push(stub)
        mocks.getColdStorageItem.mockResolvedValue({ character: restored })
        const events: string[] = []
        mocks.replacePersistentDatabase.mockImplementation(async (database) => {
            events.push('replace')
            Object.assign(mocks.database, structuredClone(database))
        })
        mocks.activateCharacter.mockImplementation(async (_id, options) => {
            const prepared = await options.prepare()
            if(prepared){
                await mocks.replacePersistentDatabase(prepared.database, prepared.reason)
            }
            events.push('activate')
            return true
        })

        const changed = await changeChar(0)

        expect(changed).toBe(true)
        expect(events).toEqual(['replace', 'activate'])
        expect(mocks.database.characters[0].name).toBe('Restored')
    })

    it('assigns an ID before a new character first chat is inserted', () => {
        const character = createBlankChar()

        expect(character.chats[0].id).toBeTruthy()
    })

    it('commits a detached scratch character before installing it', async () => {
        let installedDuringCommit = false
        mocks.commitCharacterAddition.mockImplementation(async (request) => {
            expect(mocks.database.characters).toEqual([])
            expect(request.characterId).toBeTruthy()
            request.install()
            installedDuringCommit = mocks.database.characters.length === 1
        })

        const characterId = await createNewCharacter()

        expect(installedDuringCommit).toBe(true)
        expect(characterId).toBe(mocks.database.characters[0].chaId)
        expect(mocks.database.characters[0].chats.every((chat) => chat.id)).toBe(true)
    })

    it('commits a complete detached group before installing it', async () => {
        await createNewGroup()

        expect(mocks.database.characters[0].chats[0].id).toBeTruthy()
        expect(mocks.commitCharacterAddition).toHaveBeenCalledOnce()
    })

    it('preserves an installed dirty character when addition publication fails', async () => {
        mocks.commitCharacterAddition.mockImplementation(async (request) => {
            request.install()
            throw new Error('publication failed')
        })

        await expect(createNewCharacter()).rejects.toThrow('publication failed')

        expect(mocks.database.characters).toHaveLength(1)
        expect(mocks.commitCharacterAddition).toHaveBeenCalledOnce()
    })

    it('activates the captured scratch character only after its commit succeeds', async () => {
        const events: string[] = []
        mocks.commitCharacterAddition.mockImplementation(async (request) => {
            events.push('commit')
            request.install()
        })
        mocks.activateCharacter.mockImplementation(async (id) => {
            events.push(`activate:${id}`)
            return true
        })

        await addCharacter()

        const characterId = mocks.database.characters[0].chaId
        expect(events).toEqual(['commit', `activate:${characterId}`])
    })

    it.each([
        ['local', 'createfromScratch'],
        ['official', 'createGroup'],
    ])('settles a %s addition rejection and restores the mobile stack', async (_kind, choice) => {
        mocks.alertAddCharacter.mockResolvedValue(choice)
        let installs = 0
        const failure = new Error(`${_kind} publication failed`)
        mocks.commitCharacterAddition.mockImplementation(async (request) => {
            installs++
            request.install()
            throw failure
        })

        await expect(addCharacter()).resolves.toBeUndefined()

        expect(installs).toBe(1)
        expect(mocks.database.characters).toHaveLength(1)
        expect(mocks.activateCharacter).not.toHaveBeenCalled()
        expect(mocks.alertError).toHaveBeenCalledOnce()
        expect(mocks.alertError).toHaveBeenCalledWith(failure)
        expect(get(MobileGUIStack)).toBe(1)
    })

    it('assigns an ID when formatting creates an empty-chat fallback', () => {
        const character = createBlankChar()
        character.chats = []

        const formatted = characterFormatUpdate(character)

        expect(formatted.chats[0].id).toBeTruthy()
    })
})

describe('character activation retry', () => {
    beforeEach(() => {
        mocks.database.characters = []
        mocks.nextId = 0
        vi.clearAllMocks()
        mocks.activateCharacter.mockResolvedValue(true)
    })

    it('retries activation once when the first attempt fails', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        mocks.activateCharacter.mockResolvedValueOnce(false).mockResolvedValueOnce(true)

        const changed = await changeChar(0)

        expect(changed).toBe(true)
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
    })

    it('gives up after exactly one retry', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        mocks.activateCharacter.mockResolvedValue(false)

        const changed = await changeChar(0)

        expect(changed).toBe(false)
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
    })
})

describe('chat list operations', () => {
    const buildCharacter = () => {
        const character = createBlankChar()
        character.chats = [
            { message: [], note: '', name: 'Chat A', localLore: [], id: 'chat-a' },
            { message: [], note: '', name: 'Chat B', localLore: [], id: 'chat-b' },
            { message: [], note: '', name: 'Chat C', localLore: [], id: 'chat-c' },
        ]
        character.chatPage = 1
        return character
    }

    beforeEach(() => {
        mocks.database.characters = []
        mocks.nextId = 0
        vi.clearAllMocks()
        mocks.changeChatTo.mockResolvedValue(true)
    })

    it('keeps the selected chat when another chat is removed', async () => {
        const character = buildCharacter()

        const removed = await removeChat(character, 'chat-c')

        expect(removed).toBe(true)
        expect(character.chats.map((chat: any) => chat.id)).toEqual(['chat-a', 'chat-b'])
        expect(character.chatPage).toBe(1)
        expect(mocks.changeChatTo).toHaveBeenCalledWith('chat-b')
    })

    it('moves to a surviving chat when the selected chat is removed', async () => {
        const character = buildCharacter()

        await removeChat(character, 'chat-b')

        expect(character.chats.map((chat: any) => chat.id)).toEqual(['chat-a', 'chat-c'])
        expect(character.chatPage).toBe(0)
        expect(mocks.changeChatTo).toHaveBeenCalledWith('chat-a')
    })

    it('repairs the chat page synchronously even when activation fails twice', async () => {
        const character = buildCharacter()
        character.chatPage = 2
        mocks.changeChatTo.mockResolvedValue(false)

        await removeChat(character, 'chat-c')

        expect(character.chats).toHaveLength(2)
        expect(character.chatPage).toBeLessThan(character.chats.length)
        expect(character.chats[character.chatPage]).toBeTruthy()
        expect(mocks.changeChatTo).toHaveBeenCalledTimes(2)
    })

    it('retries activation once before settling for the repaired page', async () => {
        const character = buildCharacter()
        mocks.changeChatTo.mockResolvedValueOnce(false).mockResolvedValueOnce(true)

        await removeChat(character, 'chat-b')

        expect(mocks.changeChatTo).toHaveBeenCalledTimes(2)
        expect(mocks.changeChatTo).toHaveBeenNthCalledWith(2, 'chat-a')
    })

    it('does nothing for an unknown chat id', async () => {
        const character = buildCharacter()

        const removed = await removeChat(character, 'missing')

        expect(removed).toBe(false)
        expect(character.chats).toHaveLength(3)
        expect(character.chatPage).toBe(1)
        expect(mocks.changeChatTo).not.toHaveBeenCalled()
    })

    it('adds a new chat in front and activates it', async () => {
        const character = buildCharacter()

        const added = await addNewChat(character)

        expect(added).toBe(true)
        expect(character.chats).toHaveLength(4)
        expect(character.chats[0].name).toBe('New Chat 4')
        expect(character.chats[0].id).toBeTruthy()
        expect(mocks.changeChatTo).toHaveBeenCalledWith(character.chats[0].id)
    })

    it('seeds group chats with member first messages', async () => {
        const character = buildCharacter() as any
        character.type = 'group'
        character.characters = ['member-1']
        mocks.findCharacterbyId.mockReturnValue({ firstMessage: 'hello there' })

        await addNewChat(character)

        expect(character.chats[0].message).toEqual([
            { saying: 'member-1', role: 'char', data: 'hello there' },
        ])
    })
})
