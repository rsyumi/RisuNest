import { describe, expect, it } from 'vitest'
import type { CharacterSummary } from './persistentDataStore'
import {
    createCatalogPresetWorkingSet,
    createCatalogCharacterStub,
    getCatalogCharacterMetadata,
    getCatalogConversationCount,
    getCatalogPresetMetadata,
    isCatalogCharacterStub,
    isCatalogPresetWorkingSet,
    patchWorkingSetCharacterDetail,
    projectCatalogWorkingSet,
    projectCompleteScalableWorkingSet,
} from './workingSetCatalog'
import type { Database, character } from './database.svelte'

const summary: CharacterSummary = {
    id: 'char-a',
    name: 'Alpha',
    image: 'assets/alpha.png',
    configuredIndex: 4,
    recentAt: 1_700_000_000_000,
    trashed: true,
    conversationCount: 37,
    type: 'character',
    creatorNotes: 'Catalog note',
    trashTime: 1_699_000_000_000,
}

describe('working-set catalog', () => {
    it('creates a payload-free compatibility stub with non-enumerable catalog metadata', () => {
        const stub = createCatalogCharacterStub(summary)

        expect(stub).toMatchObject({
            chaId: 'char-a',
            name: 'Alpha',
            image: 'assets/alpha.png',
            type: 'character',
            creatorNotes: 'Catalog note',
            trashTime: 1_699_000_000_000,
            lastInteraction: 1_700_000_000_000,
            chats: [],
        })
        expect(Object.keys(stub).sort()).toEqual([
            'chaId',
            'chats',
            'creatorNotes',
            'image',
            'lastInteraction',
            'name',
            'trashTime',
            'type',
        ])
        expect(JSON.stringify(stub)).not.toContain('conversationCount')
        expect(isCatalogCharacterStub(stub)).toBe(true)
        expect(getCatalogCharacterMetadata(stub)).toEqual({
            configuredIndex: 4,
            conversationCount: 37,
            residency: 'catalog',
        })
        expect(getCatalogConversationCount(stub)).toBe(37)
    })

    it('projects configured catalog order without changing the persisted root order', () => {
        const root = {
            characterOrder: [{ name: 'Folder', id: 'folder', data: ['char-a'] }, 'char-b'],
            botPresetsId: 0,
        } as unknown as Omit<Database, 'characters' | 'botPresets'>
        const beta = { ...summary, id: 'char-b', name: 'Beta', configuredIndex: 1 }
        const database = projectCatalogWorkingSet(root, [beta, summary], [])

        expect(database.characters.map((character) => character.chaId)).toEqual([
            'char-b',
            'char-a',
        ])
        expect(database.characterOrder).toEqual(root.characterOrder)
        expect(database.characterOrder).toBe(root.characterOrder)
    })

    it('uses resident chat length when a hydrated character has no catalog marker', () => {
        const hydrated = {
            chaId: 'char-a',
            chats: [{ id: 'chat-a' }, { id: 'chat-b' }],
        } as character

        expect(isCatalogCharacterStub(hydrated)).toBe(false)
        expect(getCatalogConversationCount(hydrated)).toBe(2)
    })

    it('keeps an inactive detail mutation bounded to catalog fields', () => {
        const stub = createCatalogCharacterStub(summary)

        patchWorkingSetCharacterDetail(stub, {
            type: 'character',
            chaId: 'char-a',
            name: 'Renamed',
            creatorNotes: 'Updated note',
            personality: 'must stay out of the catalog',
            systemPrompt: 'must stay out too',
        } as any)

        expect(isCatalogCharacterStub(stub)).toBe(true)
        expect(stub.chats).toEqual([])
        expect(stub).not.toHaveProperty('personality')
        expect(stub).not.toHaveProperty('systemPrompt')
        expect(stub).not.toHaveProperty('trashTime')
        expect(stub).toMatchObject({
            chaId: 'char-a',
            name: 'Renamed',
            creatorNotes: 'Updated note',
        })
        expect(getCatalogConversationCount(stub)).toBe(37)
    })

    it('keeps preset indexes stable while hydrating only the selected preset body', () => {
        const active = {
            name: 'Active',
            image: 'active.png',
            mainPrompt: 'Full active body',
        } as Database['botPresets'][number]
        const presets = createCatalogPresetWorkingSet(
            {
                revision: 7,
                items: [
                    { id: 'preset-a', name: 'Inactive', image: 'inactive.png', configuredIndex: 0 },
                    { id: 'preset-b', name: 'Active', image: 'active.png', configuredIndex: 1 },
                ],
            },
            {
                summary: { id: 'preset-b', name: 'Active', image: 'active.png', configuredIndex: 1 },
                value: active,
            },
        )

        expect(presets).toHaveLength(2)
        expect(presets[0]).toEqual({ name: 'Inactive', image: 'inactive.png' })
        expect(Object.keys(presets[0]).sort()).toEqual(['image', 'name'])
        expect(presets[1]).toBe(active)
        expect(isCatalogPresetWorkingSet(presets)).toBe(true)
        expect(getCatalogPresetMetadata(presets)).toEqual({
            activeConfiguredIndex: 1,
            catalogRevision: 7,
            residency: 'selected-only',
        })
        expect(JSON.stringify(presets)).not.toContain('selected-only')
    })

    it('projects a complete replacement to selected-only scalable state without mutating it', () => {
        const complete = {
            botPresetsId: 1,
            botPresets: [
                { name: 'Inactive', image: 'inactive.png', mainPrompt: 'inactive body' },
                { name: 'Active', image: 'active.png', mainPrompt: 'active body' },
            ],
            characters: [
                {
                    chaId: 'char-a',
                    type: 'character',
                    name: 'Alpha',
                    creatorNotes: 'Alpha note',
                    lastInteraction: 17,
                    chats: [{ id: 'chat-a', message: [{ role: 'user', data: 'resident' }] }],
                },
                {
                    chaId: 'char-b',
                    type: 'character',
                    name: 'Beta',
                    creatorNotes: 'Beta note',
                    chats: [{ id: 'chat-b', message: [{ role: 'user', data: 'released' }] }],
                },
            ],
        } as unknown as Database
        const snapshot = structuredClone(complete)

        const projected = projectCompleteScalableWorkingSet(complete, 'char-a', 12)

        expect(projected).not.toBe(complete)
        expect(projected.botPresets[0]).toEqual({ name: 'Inactive', image: 'inactive.png' })
        expect(projected.botPresets[1]).toEqual(complete.botPresets[1])
        expect(projected.botPresets[1]).not.toBe(complete.botPresets[1])
        expect(getCatalogPresetMetadata(projected.botPresets)).toMatchObject({
            activeConfiguredIndex: 1,
            catalogRevision: 12,
        })
        expect(projected.characters[0]).toEqual(complete.characters[0])
        expect(projected.characters[0]).not.toBe(complete.characters[0])
        expect(isCatalogCharacterStub(projected.characters[0])).toBe(false)
        expect(isCatalogCharacterStub(projected.characters[1])).toBe(true)
        expect(projected.characters[1].chats).toEqual([])
        expect(complete).toEqual(snapshot)
    })

    it('preserves a selected group and its hydrated member set during scalable transition', () => {
        const complete = {
            botPresetsId: 0,
            botPresets: [{ name: 'Active', mainPrompt: 'active body' }],
            characters: [
                {
                    chaId: 'member-a',
                    type: 'character',
                    name: 'Alpha',
                    personality: 'alpha body',
                    chats: [],
                },
                {
                    chaId: 'member-b',
                    type: 'character',
                    name: 'Beta',
                    personality: 'beta body',
                    chats: [],
                },
                {
                    chaId: 'group-a',
                    type: 'group',
                    name: 'Group',
                    characters: ['member-a', 'member-b'],
                    characterTalks: [1, 1],
                    characterActive: [true, true],
                    chats: [],
                },
                {
                    chaId: 'inactive',
                    type: 'character',
                    name: 'Inactive',
                    personality: 'must be released',
                    chats: [],
                },
            ],
        } as unknown as Database

        const projected = projectCompleteScalableWorkingSet(
            complete,
            'group-a',
            9,
            new Set(['group-a', 'member-a', 'member-b']),
        )

        expect(projected.characters.slice(0, 3).every(isCatalogCharacterStub)).toBe(false)
        expect(projected.characters[0].personality).toBe('alpha body')
        expect(projected.characters[1].personality).toBe('beta body')
        expect(projected.characters[2].type).toBe('group')
        expect(isCatalogCharacterStub(projected.characters[3])).toBe(true)
        expect(projected.characters[3]).not.toHaveProperty('personality')
    })
})
