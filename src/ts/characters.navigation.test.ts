import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

const mocks = vi.hoisted(() => ({
    database: { characters: [] as any[] },
    nextId: 0,
    navigationGeneration: 0,
    activateCharacter: vi.fn(async (_id?: string, _options?: any) => true),
    deactivateActiveWorkingSet: vi.fn(async () => true),
    getPersistentNavigationGeneration: vi.fn(() => mocks.navigationGeneration),
    invalidatePersistentNavigation: vi.fn(() => {
        mocks.navigationGeneration++
    }),
    commitCharacterAddition: vi.fn(async (request: any, _reason: string) => request.install()),
    markPersistentDataDirty: vi.fn(),
    mutatePersistentCharacterDetail: vi.fn(),
    materializePersistentDatabaseSnapshotWithRevision: vi.fn(),
    replacePersistentDatabase: vi.fn(),
    readPersistentCharacterDetail: vi.fn(),
    replacePersistentCompleteCharacter: vi.fn(),
    reconcilePersistentActiveCharacterIds: vi.fn(),
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
    deactivateActiveWorkingSet: mocks.deactivateActiveWorkingSet,
    getPersistentNavigationGeneration: mocks.getPersistentNavigationGeneration,
    invalidatePersistentNavigation: mocks.invalidatePersistentNavigation,
    markPersistentDataDirty: mocks.markPersistentDataDirty,
    mutatePersistentCharacterDetail: mocks.mutatePersistentCharacterDetail,
    materializePersistentDatabaseSnapshotWithRevision:
        mocks.materializePersistentDatabaseSnapshotWithRevision,
    replacePersistentDatabase: mocks.replacePersistentDatabase,
    readPersistentCharacterDetail: mocks.readPersistentCharacterDetail,
    replacePersistentCompleteCharacter: mocks.replacePersistentCompleteCharacter,
    reconcilePersistentActiveCharacterIds: mocks.reconcilePersistentActiveCharacterIds,
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
import { MobileGUIStack, OpenRealmStore, selectedCharID } from './stores.svelte'
import { doingChat } from './process/index.svelte'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

describe('runtime chat identity', () => {
    beforeEach(() => {
        mocks.database.characters = []
        mocks.nextId = 0
        mocks.navigationGeneration = 0
        OpenRealmStore.set(false)
        doingChat.set(false)
        vi.clearAllMocks()
        mocks.activateCharacter.mockResolvedValue(true)
        mocks.deactivateActiveWorkingSet.mockResolvedValue(true)
        mocks.alertConfirm.mockResolvedValue(true)
        mocks.alertAddCharacter.mockResolvedValue('createfromScratch')
        mocks.commitCharacterAddition.mockImplementation(async (request) => request.install())
        mocks.mutatePersistentCharacterDetail.mockImplementation(async (id, _reason, mutate) => {
            const index = mocks.database.characters.findIndex((character) => character.chaId === id)
            if (index < 0) return false
            const { chats: _chats, ...detail } = structuredClone(mocks.database.characters[index])
            const state = { root: {}, character: detail }
            const result = await mutate(state)
            Object.assign(mocks.database, state.root)
            if (result?.delete) mocks.database.characters.splice(index, 1)
            else Object.assign(mocks.database.characters[index], state.character)
            return true
        })
        mocks.materializePersistentDatabaseSnapshotWithRevision.mockImplementation(async () => ({
            database: structuredClone(mocks.database),
            revision: 1,
            mutationGeneration: 0,
        }))
        mocks.replacePersistentDatabase.mockImplementation(async (database) => {
            Object.assign(mocks.database, structuredClone(database))
        })
        mocks.readPersistentCharacterDetail.mockImplementation(async (id) => {
            const character = mocks.database.characters.find((candidate) => candidate.chaId === id)
            if (!character) return null
            const { chats: _chats, ...detail } = structuredClone(character)
            return detail
        })
        mocks.replacePersistentCompleteCharacter.mockImplementation(async (id, _reason, mutate) => {
            const index = mocks.database.characters.findIndex((character) => character.chaId === id)
            if (index < 0) return false
            mocks.database.characters[index] = await mutate(
                structuredClone(mocks.database.characters[index]),
            )
            return true
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

    it('commits a stable-ID character deletion through one authoritative replacement', async () => {
        const first = createBlankChar()
        const second = createBlankChar()
        mocks.database.characters.push(first, second)

        await removeChar(first.chaId, first.name, 'permanentForce')

        expect(mocks.replacePersistentDatabase).toHaveBeenCalledOnce()
        expect(mocks.replacePersistentDatabase.mock.calls[0][1]).toBe('character-removal')
        expect(mocks.database.characters.map((character: any) => character.chaId)).toEqual([second.chaId])
        expect(mocks.deactivateActiveWorkingSet).toHaveBeenCalledTimes(2)
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

        expect(mocks.replacePersistentDatabase).toHaveBeenCalledOnce()
        expect(mocks.database.characters.map((character: any) => character.chaId)).toEqual([second.chaId])
    })

    it('does not trash a selected character when safe deactivation fails', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        selectedCharID.set(0)
        mocks.deactivateActiveWorkingSet.mockResolvedValueOnce(false)

        await removeChar(character.chaId, character.name, 'normal')

        expect(mocks.database.characters[0].trashTime).toBeUndefined()
        expect(get(selectedCharID)).toBe(0)
        expect(mocks.deactivateActiveWorkingSet).toHaveBeenCalledOnce()
        expect(mocks.mutatePersistentCharacterDetail).not.toHaveBeenCalled()
    })

    it('keeps selection when the selected-character trash mutation does not commit', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        selectedCharID.set(0)
        mocks.mutatePersistentCharacterDetail.mockResolvedValueOnce(false)

        await removeChar(character.chaId, character.name, 'normal')

        expect(mocks.deactivateActiveWorkingSet).toHaveBeenCalledOnce()
        expect(mocks.mutatePersistentCharacterDetail).toHaveBeenCalledOnce()
        expect(get(selectedCharID)).toBe(0)
        expect(mocks.activateCharacter).toHaveBeenCalledWith(character.chaId)
    })

    it('deselects a released stub when busy generation blocks failure restoration', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        selectedCharID.set(0)
        const mutation = deferred<boolean>()
        mocks.mutatePersistentCharacterDetail.mockReturnValueOnce(mutation.promise)
        mocks.activateCharacter.mockResolvedValueOnce(false)

        const removal = removeChar(character.chaId, character.name, 'normal')
        await vi.waitFor(() => expect(mocks.mutatePersistentCharacterDetail).toHaveBeenCalledOnce())
        doingChat.set(true)
        mutation.resolve(false)
        await removal

        expect(mocks.activateCharacter).toHaveBeenCalledWith(character.chaId)
        expect(get(selectedCharID)).toBe(-1)
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalledWith(
            mocks.database,
            null,
        )
    })

    it('deselects a released stub when failure restoration rejects', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        selectedCharID.set(0)
        mocks.mutatePersistentCharacterDetail.mockResolvedValueOnce(false)
        mocks.activateCharacter.mockRejectedValueOnce(new Error('restore read failed'))

        await expect(
            removeChar(character.chaId, character.name, 'normal'),
        ).resolves.toBeUndefined()

        expect(get(selectedCharID)).toBe(-1)
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalledWith(
            mocks.database,
            null,
        )
    })

    it('preserves the mutation error when failure restoration also rejects', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        selectedCharID.set(0)
        mocks.mutatePersistentCharacterDetail.mockRejectedValueOnce(
            new Error('mutation failed'),
        )
        mocks.activateCharacter.mockRejectedValueOnce(new Error('restore read failed'))

        await expect(
            removeChar(character.chaId, character.name, 'normal'),
        ).rejects.toThrow('mutation failed')

        expect(get(selectedCharID)).toBe(-1)
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalledWith(
            mocks.database,
            null,
        )
    })

    it('does not restore an old selection after a newer navigation wins', async () => {
        const first = createBlankChar()
        const second = createBlankChar()
        mocks.database.characters.push(first, second)
        selectedCharID.set(0)
        const mutation = deferred<boolean>()
        mocks.mutatePersistentCharacterDetail.mockReturnValueOnce(mutation.promise)

        const removal = removeChar(first.chaId, first.name, 'normal')
        await vi.waitFor(() => expect(mocks.mutatePersistentCharacterDetail).toHaveBeenCalledOnce())
        mocks.navigationGeneration++
        selectedCharID.set(1)
        mutation.resolve(false)
        await removal

        expect(get(selectedCharID)).toBe(1)
        expect(mocks.activateCharacter).not.toHaveBeenCalled()
    })

    it('does not clear a newer selection when delayed removal succeeds', async () => {
        const first = createBlankChar()
        const second = createBlankChar()
        mocks.database.characters.push(first, second)
        selectedCharID.set(0)
        const mutation = deferred<boolean>()
        mocks.mutatePersistentCharacterDetail.mockReturnValueOnce(mutation.promise)

        const removal = removeChar(first.chaId, first.name, 'normal')
        await vi.waitFor(() => expect(mocks.mutatePersistentCharacterDetail).toHaveBeenCalledOnce())
        mocks.navigationGeneration++
        selectedCharID.set(1)
        mutation.resolve(true)
        await removal

        expect(get(selectedCharID)).toBe(1)
        expect(mocks.reconcilePersistentActiveCharacterIds).not.toHaveBeenCalled()
    })

    it('clears selection only after the selected-character trash mutation commits', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        selectedCharID.set(0)

        await removeChar(character.chaId, character.name, 'normal')

        expect(mocks.deactivateActiveWorkingSet.mock.invocationCallOrder[0]).toBeLessThan(
            mocks.mutatePersistentCharacterDetail.mock.invocationCallOrder[0],
        )
        expect(mocks.database.characters[0].trashTime).toEqual(expect.any(Number))
        expect(get(selectedCharID)).toBe(-1)
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalledWith(
            mocks.database,
            null,
        )
    })

    it('keeps a locally committed trash when official publication fails', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        selectedCharID.set(0)
        mocks.mutatePersistentCharacterDetail.mockImplementationOnce(
            async (id, _reason, mutate) => {
                const target = mocks.database.characters.find((candidate) => candidate.chaId === id)
                const { chats: _chats, ...detail } = structuredClone(target)
                await mutate({ root: {}, character: detail })
                Object.assign(target, detail)
                throw new Error('official publish failed')
            },
        )

        await expect(
            removeChar(character.chaId, character.name, 'normal'),
        ).rejects.toThrow('official publish failed')

        expect(mocks.database.characters[0].trashTime).toEqual(expect.any(Number))
        expect(get(selectedCharID)).toBe(-1)
        expect(mocks.activateCharacter).not.toHaveBeenCalled()
    })

    it('persists selected-group cleanup only after permanent member deletion succeeds', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['member-a', 'member-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [],
        }
        const memberA = createBlankChar()
        memberA.chaId = 'member-a'
        const memberB = createBlankChar()
        memberB.chaId = 'member-b'
        mocks.database.characters.push(group, memberA, memberB)
        selectedCharID.set(0)

        await removeChar('member-a', memberA.name, 'permanentForce')

        expect(mocks.replacePersistentDatabase).toHaveBeenCalledOnce()
        const updatedGroup = mocks.database.characters.find(
            (character) => character.chaId === 'group-a',
        )
        expect(updatedGroup.characters).toEqual(['member-b'])
        expect(updatedGroup.characterTalks).toEqual([0.75])
        expect(updatedGroup.characterActive).toEqual([true])
        expect(mocks.deactivateActiveWorkingSet).toHaveBeenCalledTimes(2)
    })

    it('keeps permanent deletion and every group reference locally consistent when publication fails', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            characters: ['member-a', 'member-b'],
            characterTalks: [0.25, 0.75],
            characterActive: [false, true],
            chats: [],
        }
        const memberA = createBlankChar()
        memberA.chaId = 'member-a'
        const memberB = createBlankChar()
        memberB.chaId = 'member-b'
        mocks.database.characters.push(group, memberA, memberB)
        selectedCharID.set(0)
        mocks.replacePersistentDatabase.mockImplementationOnce(async (database) => {
            Object.assign(mocks.database, structuredClone(database))
            throw new Error('official publish failed')
        })

        await expect(
            removeChar('member-a', memberA.name, 'permanentForce'),
        ).rejects.toThrow('official publish failed')

        expect(mocks.database.characters.some((character) => character.chaId === 'member-a')).toBe(false)
        const updatedGroup = mocks.database.characters.find(
            (character) => character.chaId === 'group-a',
        )
        expect(updatedGroup.characters).toEqual(['member-b'])
        expect(updatedGroup.characterTalks).toEqual([0.75])
        expect(updatedGroup.characterActive).toEqual([true])
        expect(mocks.activateCharacter).not.toHaveBeenCalled()
        expect(mocks.deactivateActiveWorkingSet).toHaveBeenCalledTimes(2)
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
        mocks.replacePersistentCompleteCharacter.mockImplementation(async (id, _reason, mutate) => {
            events.push('replace')
            const index = mocks.database.characters.findIndex((character) => character.chaId === id)
            mocks.database.characters[index] = await mutate(mocks.database.characters[index])
            return true
        })
        mocks.activateCharacter.mockImplementation(async () => {
            events.push('activate')
            return true
        })

        const changed = await changeChar(0)

        expect(changed).toBe(true)
        expect(events).toEqual(['replace', 'activate'])
        expect(mocks.database.characters[0].name).toBe('Restored')
    })

    it('restores cold group members before activating the group', async () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['member-a'],
            chats: [],
        }
        const member = createBlankChar()
        member.chaId = 'member-a'
        member.name = 'Cold member'
        member.coldstorage = 'member-cold-key'
        const restored = structuredClone(member)
        restored.name = 'Restored member'
        delete restored.coldstorage
        mocks.database.characters.push(group, member)
        mocks.getColdStorageItem.mockResolvedValue({ character: restored })

        expect(await changeChar(0)).toBe(true)

        expect(mocks.replacePersistentCompleteCharacter).toHaveBeenCalledWith(
            'member-a',
            'cold-character-restore',
            expect.any(Function),
        )
        expect(mocks.database.characters[1].name).toBe('Restored member')
        expect(mocks.activateCharacter).toHaveBeenCalledWith('group-a', undefined)
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
        mocks.navigationGeneration = 0
        OpenRealmStore.set(false)
        vi.clearAllMocks()
        mocks.deactivateActiveWorkingSet.mockResolvedValue(true)
        mocks.activateCharacter.mockReset()
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return true
        })
        mocks.readPersistentCharacterDetail.mockImplementation(async (id) => {
            const character = mocks.database.characters.find((candidate) => candidate.chaId === id)
            if (!character) return null
            const { chats: _chats, ...detail } = structuredClone(character)
            return detail
        })
    })

    it('deactivates the working set before opening the Realm catalog', async () => {
        mocks.alertAddCharacter.mockResolvedValue('importFromRealm')

        await addCharacter()

        expect(mocks.deactivateActiveWorkingSet).toHaveBeenCalledOnce()
    })

    it('keeps the current destination when Realm deactivation fails', async () => {
        mocks.alertAddCharacter.mockResolvedValue('importFromRealm')
        mocks.deactivateActiveWorkingSet.mockResolvedValueOnce(false)

        await addCharacter()

        expect(get(OpenRealmStore)).toBe(false)
    })

    it('does not let a slow cold character restore override a newer character navigation', async () => {
        const first = createBlankChar()
        first.coldstorage = 'first-cold-key'
        const second = createBlankChar()
        mocks.database.characters.push(first, second)
        const firstDetail = deferred<any>()
        mocks.readPersistentCharacterDetail.mockImplementation(async (id) => {
            if (id === first.chaId) return firstDetail.promise
            const character = mocks.database.characters.find((candidate) => candidate.chaId === id)
            if (!character) return null
            const { chats: _chats, ...detail } = structuredClone(character)
            return detail
        })

        const older = changeChar(0)
        await vi.waitFor(() => expect(mocks.readPersistentCharacterDetail).toHaveBeenCalledWith(
            first.chaId,
            'cold-character-inspection',
        ))

        await expect(changeChar(1)).resolves.toBe(true)
        const { chats: _chats, ...detail } = structuredClone(first)
        firstDetail.resolve(detail)

        await expect(older).resolves.toBe(false)
        expect(mocks.activateCharacter.mock.calls.map(([id]) => id)).toEqual([second.chaId])
        expect(mocks.getColdStorageItem).not.toHaveBeenCalled()
    })

    it('does not let a slow cold character restore override Home navigation', async () => {
        const character = createBlankChar()
        character.coldstorage = 'cold-key'
        mocks.database.characters.push(character)
        const detailRead = deferred<any>()
        mocks.readPersistentCharacterDetail.mockReturnValue(detailRead.promise)

        const pending = changeChar(0)
        await vi.waitFor(() => expect(mocks.readPersistentCharacterDetail).toHaveBeenCalledOnce())
        mocks.invalidatePersistentNavigation()
        const { chats: _chats, ...detail } = structuredClone(character)
        detailRead.resolve(detail)

        await expect(pending).resolves.toBe(false)
        expect(mocks.activateCharacter).not.toHaveBeenCalled()
        expect(mocks.getColdStorageItem).not.toHaveBeenCalled()
    })

    it('does not retry an activation superseded by newer character navigation', async () => {
        const first = createBlankChar()
        const second = createBlankChar()
        mocks.database.characters.push(first, second)
        const firstActivation = deferred<boolean>()
        mocks.activateCharacter.mockImplementation(async (id) => {
            mocks.navigationGeneration++
            if (id === first.chaId) return firstActivation.promise
            return true
        })

        const older = changeChar(0)
        await vi.waitFor(() => expect(mocks.activateCharacter).toHaveBeenCalledWith(first.chaId, undefined))
        await expect(changeChar(1)).resolves.toBe(true)
        firstActivation.resolve(false)

        await expect(older).resolves.toBe(false)
        expect(mocks.activateCharacter.mock.calls.map(([id]) => id)).toEqual([
            first.chaId,
            second.chaId,
        ])
    })

    it('retries activation once when the first attempt fails', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        const results = [false, true]
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return results.shift() ?? false
        })

        const changed = await changeChar(0)

        expect(changed).toBe(true)
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
    })

    it('gives up after exactly one retry', async () => {
        const character = createBlankChar()
        mocks.database.characters.push(character)
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return false
        })

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
