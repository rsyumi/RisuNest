import { describe, expect, it } from 'vitest'
import type { CharacterSummary } from './persistentDataStore'
import { createCatalogPresetWorkingSet, carryCatalogCharacterMetadata, createCatalogCharacterStub, getCatalogCharacterMetadata, getCatalogConversationCount, getCatalogPresetMetadata, isArchivedCharacter, isCatalogCharacterStub, isCatalogPresetWorkingSet, isWorkingSetCharacterStub, patchWorkingSetCharacterDetail, patchWorkingSetRoot, projectCatalogWorkingSet, projectCompleteScalableWorkingSet } from './workingSetCatalog'
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
    it('retains catalog eligibility after publishing hydrated detail', () => {
        const previous = createCatalogCharacterStub(summary)
        const replacement = { ...previous, chats: [{ id: 'chat-a', name: 'Chat A', note: '', localLore: [], message: [] }] }
        expect(getCatalogCharacterMetadata(replacement)).toBeUndefined()
        expect(carryCatalogCharacterMetadata(previous, replacement)).toBe(replacement)
        expect(getCatalogCharacterMetadata(replacement)).toMatchObject({ residency: 'detail', conversationCount: 1 })
        expect(JSON.stringify(replacement)).not.toContain('residency')
    })

    it('publishes changed and removed root fields without disturbing separately owned data', () => {
        const database = {
            username: 'Before', userNote: 'Removed',
            promptTemplate: [{ type: 'plain', text: 'Before' }],
            characters: [], botPresets: [], pluginCustomStorage: { synthetic: true }, pluginStorageMeta: {},
        } as unknown as Database
        const { characters, botPresets, pluginCustomStorage, pluginStorageMeta } = database
        patchWorkingSetRoot(database, {
            username: 'After', promptTemplate: [{ type: 'plain', text: 'After' }],
        } as any)
        expect(database.username).toBe('After')
        expect(database.promptTemplate[0]).toMatchObject({ text: 'After' })
        expect(Object.hasOwn(database, 'userNote')).toBe(false)
        expect(database.characters).toBe(characters)
        expect(database.botPresets).toBe(botPresets)
        expect(database.pluginCustomStorage).toBe(pluginCustomStorage)
        expect(database.pluginStorageMeta).toBe(pluginStorageMeta)
    })

    it('marks an archived summary as archived residency and never as a catalog stub', () => {
        const stub = createCatalogCharacterStub({
            ...summary,
            conversationCount: 0,
            archived: { archivedAt: 1_758_000_000_000, conversationCount: 12, messageCount: 412 },
        })

        expect(getCatalogCharacterMetadata(stub)).toEqual({
            configuredIndex: 4,
            conversationCount: 12,
            residency: 'archived',
            archivedAt: 1_758_000_000_000,
            archivedMessageCount: 412,
        })
        expect(isArchivedCharacter(stub)).toBe(true)
        expect(isCatalogCharacterStub(stub)).toBe(false)
        expect(isWorkingSetCharacterStub(stub)).toBe(true)
        expect(getCatalogConversationCount(stub)).toBe(12)
    })

    it('keeps an unarchived summary on catalog residency', () => {
        const stub = createCatalogCharacterStub(summary)

        expect(isArchivedCharacter(stub)).toBe(false)
        expect(isCatalogCharacterStub(stub)).toBe(true)
        expect(isWorkingSetCharacterStub(stub)).toBe(true)
    })

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
        expect(database.pluginCustomStorage).toEqual({})
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

    it('keeps unchanged detail values when patching a resident character', () => {
        const customscript = [{ comment: 'rule', in: 'a', out: 'b', type: 'editdisplay' }]
        const emotionImages = [['happy', 'assets/happy.png']]
        const resident = {
            type: 'character',
            chaId: 'char-a',
            name: 'Alpha',
            removedQuotes: false,
            customscript,
            emotionImages,
            chats: [],
        } as any

        patchWorkingSetCharacterDetail(resident, {
            type: 'character',
            chaId: 'char-a',
            name: 'Alpha',
            removedQuotes: true,
            customscript: structuredClone(customscript),
            emotionImages: [['sad', 'assets/sad.png']],
        } as any)

        expect(resident.removedQuotes).toBe(true)
        expect(resident.customscript).toBe(customscript)
        expect(resident.emotionImages).not.toBe(emotionImages)
        expect(resident.emotionImages).toEqual([['sad', 'assets/sad.png']])
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
        expect(presets[0]).toEqual({ id: 'preset-a', name: 'Inactive', image: 'inactive.png' })
        expect(Object.keys(presets[0]).sort()).toEqual(['id', 'image', 'name'])
        expect(presets[1]).toBe(active)
        expect(isCatalogPresetWorkingSet(presets)).toBe(true)
        expect(getCatalogPresetMetadata(presets)).toEqual({
            activeConfiguredIndex: 1,
            catalogRevision: 7,
            residency: 'selected-only',
            loaded: new Set(['preset-b']),
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
                    chatPage: 1,
                    chats: [
                        {
                            id: 'chat-a1',
                            name: 'Older',
                            note: '',
                            localLore: [],
                            message: [{ role: 'user', data: 'released' }],
                        },
                        {
                            id: 'chat-a2',
                            name: 'Selected',
                            note: '',
                            localLore: [],
                            message: [{ role: 'user', data: 'resident' }],
                        },
                    ],
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

        const projected = projectCompleteScalableWorkingSet(
            complete,
            'char-a',
            12,
            undefined,
            'chat-a2',
        )

        expect(projected).not.toBe(complete)
        expect(projected.botPresets[0]).toEqual({ name: 'Inactive', image: 'inactive.png' })
        expect(projected.botPresets[1]).toEqual(complete.botPresets[1])
        expect(projected.botPresets[1]).not.toBe(complete.botPresets[1])
        expect(getCatalogPresetMetadata(projected.botPresets)).toMatchObject({
            activeConfiguredIndex: 1,
            catalogRevision: 12,
        })
        expect(projected.characters[0]).toEqual({
            ...complete.characters[0],
            chats: [
                { ...complete.characters[0].chats[0], lastDate: 0, message: [] },
                complete.characters[0].chats[1],
            ],
        })
        expect(projected.characters[0]).not.toBe(complete.characters[0])
        expect(isCatalogCharacterStub(projected.characters[0])).toBe(false)
        expect(isCatalogCharacterStub(projected.characters[1])).toBe(true)
        expect(projected.characters[1].chats).toEqual([])
        expect(complete).toEqual(snapshot)
    })
})
