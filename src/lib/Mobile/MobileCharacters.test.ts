// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { DBState } from 'src/ts/stores.svelte'
import MobileCharacters from './MobileCharacters.svelte'

class TestIntersectionObserver {
    static instance: TestIntersectionObserver | undefined
    readonly observed = new Set<Element>()

    constructor(private readonly callback: IntersectionObserverCallback) {
        TestIntersectionObserver.instance = this
    }

    observe(target: Element) {
        this.observed.add(target)
    }

    disconnect() {
        this.observed.clear()
    }

    setVisible(target: Element) {
        this.callback(
            [
                {
                    target,
                    isIntersecting: true,
                } as IntersectionObserverEntry,
            ],
            this as unknown as IntersectionObserver,
        )
    }
}

const catalogMocks = vi.hoisted(() => ({
    getCatalogConversationCount: vi.fn((character: { chats: unknown[] }) => character.chats.length),
}))
const characterMocks = vi.hoisted(() => ({
    addCharacter: vi.fn(),
    changeChar: vi.fn(async () => false),
    getCharImage: vi.fn(() => ''),
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
vi.mock('src/ts/characters', () => characterMocks)

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
        characterMocks.addCharacter.mockClear()
        characterMocks.changeChar.mockClear()
        characterMocks.getCharImage.mockClear()
        TestIntersectionObserver.instance = undefined
        vi.unstubAllGlobals()
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

    it('reveals fallback pages in batches while preserving catalog order', async () => {
        vi.stubGlobal('IntersectionObserver', undefined)
        DBState.db.characters = Array.from({ length: 100 }, (_, index) =>
            summary(`char-${index}`, `Character ${index}`, index),
        )
        const target = document.createElement('div')
        document.body.append(target)

        mounted = mount(MobileCharacters, {
            target,
            props: { search: '', hideTrash: true },
        })
        await tick()

        expect(target.querySelectorAll('[data-character-id]')).toHaveLength(48)
        expect(characterMocks.getCharImage).toHaveBeenCalledTimes(48)
        const loadMore = target.querySelector<HTMLButtonElement>(
            '[data-load-more-sentinel] button',
        )
        expect(loadMore).not.toBeNull()
        loadMore?.click()
        await tick()

        const rows = [...target.querySelectorAll('[data-character-id]')]
        expect(rows).toHaveLength(96)
        expect(characterMocks.getCharImage).toHaveBeenCalledTimes(96)
        expect(rows[0].getAttribute('data-character-id')).toBe('char-99')
        expect(rows[95].getAttribute('data-character-id')).toBe('char-4')
        const firstRow = rows[0] as HTMLButtonElement
        firstRow.click()
        await tick()
        expect(characterMocks.changeChar).toHaveBeenCalledWith(99)
    })

    it('automatically reveals the next page when the sentinel enters view', async () => {
        vi.stubGlobal('IntersectionObserver', TestIntersectionObserver)
        DBState.db.characters = Array.from({ length: 100 }, (_, index) =>
            summary(`char-${index}`, `Character ${index}`, index),
        )
        const target = document.createElement('div')
        document.body.append(target)

        mounted = mount(MobileCharacters, {
            target,
            props: { search: '', hideTrash: true },
        })
        await tick()

        const sentinel = target.querySelector('[data-load-more-sentinel]')!
        expect(target.querySelectorAll('[data-character-id]')).toHaveLength(48)
        expect(sentinel.querySelector('button')).toBeNull()

        TestIntersectionObserver.instance?.setVisible(sentinel)
        await tick()

        expect(target.querySelectorAll('[data-character-id]')).toHaveLength(96)
    })
})
