// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { DBState } from 'src/ts/stores.svelte'
import MobileCharacters from './MobileCharacters.svelte'

const catalogMocks = vi.hoisted(() => ({
    getCatalogConversationCount: vi.fn((character: { chats: unknown[] }) => character.chats.length),
}))
const storeMocks = vi.hoisted(() => ({
    DBState: { db: { characters: [] } },
    MobileSearch: {
        subscribe(run: (value: string) => void) {
            run('')
            return () => {}
        },
    },
}))

vi.mock('src/ts/storage/workingSetCatalog', () => catalogMocks)
vi.mock('src/ts/stores.svelte', () => storeMocks)
vi.mock('src/ts/characters', () => ({
    addCharacter: vi.fn(),
    changeChar: vi.fn(async () => false),
    getCharImage: vi.fn(() => ''),
}))

describe('MobileCharacters', () => {
    let mounted: ReturnType<typeof mount> | null = null
    const originalCharacters = DBState.db.characters

    const summary = (
        chaId: string,
        name: string,
        lastInteraction: number,
        trashTime?: number,
    ) => ({
        chaId,
        type: 'character',
        name,
        image: '',
        chats: [],
        chatPage: 0,
        lastInteraction,
        trashTime,
    }) as unknown as typeof DBState.db.characters[number]

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = null
        DBState.db.characters = originalCharacters
        catalogMocks.getCatalogConversationCount.mockClear()
        document.body.replaceChildren()
    })

    it('filters 10,000 summaries before projecting the matching character once', async () => {
        DBState.db.characters = Array.from({ length: 10_000 }, (_, index) => summary(
            `char-${index}`,
            index === 9_876 ? 'Needle Character' : `Character ${index}`,
            index,
        ))
        const target = document.createElement('div')
        document.body.append(target)

        mounted = mount(MobileCharacters, {
            target,
            props: { search: 'needle', hideTrash: true },
        })
        await tick()

        const rows = target.querySelectorAll('[data-character-id]')
        expect(rows).toHaveLength(1)
        expect(rows[0].getAttribute('data-character-id')).toBe('char-9876')
        expect(catalogMocks.getCatalogConversationCount).toHaveBeenCalledTimes(1)
        expect(catalogMocks.getCatalogConversationCount).toHaveBeenCalledWith(
            DBState.db.characters[9_876],
        )
    })

    it('excludes trash and sorts interaction ties by name', async () => {
        DBState.db.characters = [
            summary('char-z', 'Zulu', 5),
            summary('char-b', 'Beta', 10),
            summary('char-a', 'Alpha', 10),
            summary('char-trash', 'Trash', 20, 1),
        ]
        const target = document.createElement('div')
        document.body.append(target)

        mounted = mount(MobileCharacters, {
            target,
            props: { search: '', hideTrash: true },
        })
        await tick()

        expect([...target.querySelectorAll('[data-character-id]')].map(
            (row) => row.getAttribute('data-character-id'),
        )).toEqual(['char-a', 'char-b', 'char-z'])
    })
})
