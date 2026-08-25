import { beforeEach, describe, expect, it, vi } from 'vitest'
import mobileHeaderSource from './Mobile/MobileHeader.svelte?raw'
import playgroundMenuSource from './Playground/PlaygroundMenu.svelte?raw'
import sidebarSource from './SideBars/Sidebar.svelte?raw'

const mocks = vi.hoisted(() => {
    let selected = 0
    const events: string[] = []
    const database = { characters: [] as any[] }
    return {
        database,
        events,
        selectedCharID: {
            subscribe(run: (value: number) => void) {
                run(selected)
                return () => undefined
            },
            set(value: number) {
                selected = value
                events.push(`select:${value}`)
            },
        },
        deactivateActiveWorkingSet: vi.fn(async () => {
            events.push('deactivate')
            return true
        }),
        markPersistentDataDirty: vi.fn(() => events.push('dirty')),
        changeChar: vi.fn(async (index: number) => {
            events.push(`activate:${index}`)
            const stub = database.characters[index]
            database.characters[index] = {
                ...stub,
                name: 'Hydrated',
                chats: [{ id: 'chat-a', message: [] }],
            }
            return true
        }),
        commitDetachedCharacter: vi.fn(async (character: any) => {
            events.push('commit')
            database.characters.push(character)
            return character.chaId
        }),
        characterFormatUpdate: vi.fn((value: number | any) => {
            events.push('format')
            return typeof value === 'number' ? database.characters[value] : value
        }),
        createBlankChar: vi.fn(() => ({
            type: 'character',
            chaId: 'new-playground',
            name: '',
            firstMessage: '',
            chats: [{ id: 'new-chat', message: [] }],
        })),
    }
})

vi.mock('src/ts/stores.svelte', () => ({
    DBState: { db: mocks.database },
    selectedCharID: mocks.selectedCharID,
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    deactivateActiveWorkingSet: mocks.deactivateActiveWorkingSet,
    markPersistentDataDirty: mocks.markPersistentDataDirty,
}))
vi.mock('src/ts/characters', () => ({
    changeChar: mocks.changeChar,
    characterFormatUpdate: mocks.characterFormatUpdate,
    commitDetachedCharacter: mocks.commitDetachedCharacter,
    createBlankChar: mocks.createBlankChar,
}))
vi.mock('src/ts/util', () => ({
    findCharacterIndexbyId: (id: string) =>
        mocks.database.characters.findIndex((character) => character.chaId === id),
}))

import { activatePlaygroundCharacter, clearCharacterSelection } from './workingSetNavigation'

describe('working-set UI navigation', () => {
    beforeEach(() => {
        mocks.database.characters = []
        mocks.events.length = 0
        vi.clearAllMocks()
    })

    it('deactivates and flushes the working set before clearing selection', async () => {
        await expect(clearCharacterSelection()).resolves.toBe(true)

        expect(mocks.events).toEqual(['deactivate', 'select:-1'])
    })

    it('keeps selection when working-set deactivation fails', async () => {
        mocks.deactivateActiveWorkingSet.mockResolvedValueOnce(false)

        await expect(clearCharacterSelection()).resolves.toBe(false)

        expect(mocks.events).toEqual([])

        await expect(clearCharacterSelection()).resolves.toBe(true)
        expect(mocks.events).toEqual(['deactivate', 'select:-1'])
    })

    it('hydrates an existing playground stub before configuring it', async () => {
        mocks.database.characters.push({
            type: 'character',
            chaId: '§playground',
            name: 'Released stub',
            chats: [],
        })

        await expect(activatePlaygroundCharacter()).resolves.toBe(true)

        expect(mocks.events).toEqual(['activate:0', 'format', 'dirty'])
        expect(mocks.database.characters[0]).toMatchObject({
            chaId: '§playground',
            name: 'assistant',
            firstMessage: '{{none}}',
            utilityBot: true,
            chats: [{ id: 'chat-a' }],
        })
    })

    it('commits a missing playground character before activating it', async () => {
        await expect(activatePlaygroundCharacter()).resolves.toBe(true)

        expect(mocks.events).toEqual(['format', 'commit', 'activate:0', 'format', 'dirty'])
        expect(mocks.commitDetachedCharacter).toHaveBeenCalledWith(
            expect.objectContaining({
                chaId: '§playground',
                name: 'assistant',
                utilityBot: true,
            }),
            'create-playground-character',
        )
        expect(mocks.changeChar).toHaveBeenCalledWith(0)
    })

    it('wires direct component navigation through the working-set helpers', () => {
        expect(mobileHeaderSource.match(/clearCharacterSelection\(\)/g)).toHaveLength(1)
        expect(sidebarSource.match(/clearCharacterSelection\(\)/g)).toHaveLength(6)
        expect(mobileHeaderSource).toContain('await clearCharacterSelection()')
        expect(sidebarSource.match(/await clearCharacterSelection\(\)/g)).toHaveLength(6)
        expect(sidebarSource).not.toContain('selectedCharID.set(-1)')
        expect(playgroundMenuSource).toContain('await activatePlaygroundCharacter()')
        expect(playgroundMenuSource).not.toContain('selectedCharID.set(charIndex)')
    })
})
