import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import type {
    CharacterSummary,
    ContentChangeKey,
    ConversationSummary,
    PersistentRevisionReader,
} from './persistentDataStore'
import {
    applyTargetedWorkingSetInvalidation,
    TARGETED_INVALIDATION_KEY_LIMIT,
    type TargetedWorkingSetInvalidationOptions,
} from './targetedWorkingSetInvalidation'
import { getCatalogPresetMetadata, projectPinnedScalableWorkingSet } from './workingSetCatalog'

vi.mock('./database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No global database in targeted invalidation tests')
    },
    presetTemplate: {},
}))
vi.mock('../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

interface ModelMessage {
    role: string
    data: string
    chatId: string
}

interface ModelConversation {
    id: string
    name: string
    configuredIndex: number
    recentAt: number
    messages: ModelMessage[]
}

interface ModelCharacter {
    id: string
    configuredIndex: number
    detail: Record<string, unknown>
    conversations: ModelConversation[]
    archived?: { archivedAt: number; conversationCount: number; messageCount: number }
}

interface Model {
    revision: number
    root: Record<string, unknown>
    presets: Array<{ name: string; image?: string; body: string }>
    characters: ModelCharacter[]
}

function clone<T>(value: T): T {
    return structuredClone(value)
}

function characterOf(model: Model, id: string): ModelCharacter | undefined {
    return model.characters.find((candidate) => candidate.id === id)
}

function summaryOf(character: ModelCharacter): CharacterSummary {
    const detail = character.detail
    return {
        id: character.id,
        name: detail.name as string,
        image: detail.image as string | undefined,
        configuredIndex: character.configuredIndex,
        recentAt: (detail.lastInteraction as number | undefined) ?? 0,
        trashed: detail.trashTime !== undefined,
        conversationCount:
            character.archived?.conversationCount ?? character.conversations.length,
        type: detail.type as CharacterSummary['type'],
        creatorNotes: (detail.creatorNotes as string | undefined) ?? '',
        trashTime: detail.trashTime as number | undefined,
        ...(character.archived === undefined ? {} : { archived: clone(character.archived) }),
    }
}

function conversationSummaryOf(
    characterId: string,
    conversation: ModelConversation,
): ConversationSummary {
    return {
        id: conversation.id,
        characterId,
        name: conversation.name,
        configuredIndex: conversation.configuredIndex,
        recentAt: conversation.recentAt,
        messageCount: conversation.messages.length,
    }
}

function pageOf<T>(
    items: T[],
    input: { limit: number; cursor?: string },
    revision: number,
): { revision: number; items: T[]; nextCursor?: string } {
    const offset = input.cursor === undefined ? 0 : Number(input.cursor)
    const slice = items.slice(offset, offset + input.limit)
    const consumed = offset + slice.length
    return {
        revision,
        items: slice,
        ...(consumed < items.length ? { nextCursor: String(consumed) } : {}),
    }
}

function unsupported(name: string): never {
    throw new Error(`${name} is not part of the scalable working-set projection`)
}

/// Mirrors what the native store hands a pinned reader, including the explicit
/// failure an archived character's detail read produces.
function createReader(model: Model): PersistentRevisionReader {
    const revision = model.revision
    return {
        revision,
        readRoot: async () => ({ revision, value: clone(model.root) }),
        queryPresets: async () => ({
            revision,
            items: model.presets.map((preset, configuredIndex) => ({
                id: String(configuredIndex),
                configuredIndex,
                name: preset.name,
                ...(preset.image === undefined ? {} : { image: preset.image }),
            })),
        }),
        readPreset: async (id: string) => {
            const preset = model.presets[Number(id)]
            return preset ? { revision, value: clone(preset) } : null
        },
        queryCharacters: async (input) =>
            pageOf(
                model.characters
                    .filter((candidate) => candidate.detail.trashTime !== undefined === input.trash)
                    .slice()
                    .sort((left, right) => left.configuredIndex - right.configuredIndex)
                    .map(summaryOf),
                input,
                revision,
            ),
        readCharacterSummary: async (id: string) => {
            const character = characterOf(model, id)
            return character ? summaryOf(character) : null
        },
        readCharacter: async (id: string) => {
            const character = characterOf(model, id)
            if (!character) return null
            if (character.archived) throw new Error(`Character ${id} is archived`)
            return { revision, value: clone(character.detail) }
        },
        queryConversations: async (input) => {
            const character = characterOf(model, input.characterId)
            return pageOf(
                (character?.conversations ?? [])
                    .slice()
                    .sort((left, right) => left.configuredIndex - right.configuredIndex)
                    .map((conversation) =>
                        conversationSummaryOf(input.characterId, conversation)),
                input,
                revision,
            )
        },
        readConversation: async (characterId: string, conversationId: string) => {
            const conversation = characterOf(model, characterId)?.conversations.find(
                (candidate) => candidate.id === conversationId,
            )
            if (!conversation) return null
            return {
                revision,
                value: clone({
                    id: conversation.id,
                    name: conversation.name,
                    note: '',
                    localLore: [],
                    message: conversation.messages,
                    lastDate: conversation.recentAt,
                }),
            }
        },
        readConversationMetadata: async () => unsupported('readConversationMetadata'),
        readConversationWindow: async () => unsupported('readConversationWindow'),
        queryPluginStorage: async () => unsupported('queryPluginStorage'),
        readPluginStorage: async () => unsupported('readPluginStorage'),
        readAssetAlias: async () => unsupported('readAssetAlias'),
        readAssetAliasesByKeys: async () => unsupported('readAssetAliasesByKeys'),
        listAssetAliases: async () => unsupported('listAssetAliases'),
        readAssetRepositoryAuthority: async () => unsupported('readAssetRepositoryAuthority'),
        readAssetOwnerHead: async () => unsupported('readAssetOwnerHead'),
    } as unknown as PersistentRevisionReader
}

/// Non-enumerable symbol payloads carry residency, configured position and the
/// conversation message count, so equality has to reach them.
function liftHiddenState(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(liftHiddenState)
    if (value === null || typeof value !== 'object') return value
    const lifted: Record<string, unknown> = {}
    for (const [key, entry] of Object.entries(value)) lifted[key] = liftHiddenState(entry)
    for (const symbol of Object.getOwnPropertySymbols(value)) {
        lifted[`«${String(symbol.description)}»`] = liftHiddenState(
            (value as Record<symbol, unknown>)[symbol],
        )
    }
    return lifted
}

function newModel(): Model {
    return {
        revision: 1,
        root: {
            username: 'Tester',
            botPresetsId: 0,
            theme: 'light',
            modules: [{ id: 'module-one', assets: [] }],
        },
        presets: [
            { name: 'First', body: 'first' },
            { name: 'Second', image: 'preset-image', body: 'second' },
        ],
        characters: [
            {
                id: 'char-a',
                configuredIndex: 0,
                detail: {
                    chaId: 'char-a',
                    name: 'Alpha',
                    type: 'character',
                    chatPage: 1,
                    lastInteraction: 10,
                    creatorNotes: 'alpha notes',
                },
                conversations: [
                    {
                        id: 'conv-a1',
                        name: 'A One',
                        configuredIndex: 0,
                        recentAt: 5,
                        messages: [{ role: 'user', data: 'hello', chatId: 'm1' }],
                    },
                    {
                        id: 'conv-a2',
                        name: 'A Two',
                        configuredIndex: 1,
                        recentAt: 9,
                        messages: [{ role: 'user', data: 'second', chatId: 'm2' }],
                    },
                ],
            },
            {
                id: 'char-b',
                configuredIndex: 1,
                detail: {
                    chaId: 'char-b',
                    name: 'Beta',
                    type: 'character',
                    chatPage: 0,
                    lastInteraction: 20,
                },
                conversations: [
                    {
                        id: 'conv-b1',
                        name: 'B One',
                        configuredIndex: 0,
                        recentAt: 20,
                        messages: [],
                    },
                ],
            },
            {
                id: 'group-g',
                configuredIndex: 2,
                detail: {
                    chaId: 'group-g',
                    name: 'Group',
                    type: 'group',
                    chatPage: 0,
                    characters: ['char-a', 'char-b'],
                },
                conversations: [
                    {
                        id: 'conv-g1',
                        name: 'G One',
                        configuredIndex: 0,
                        recentAt: 30,
                        messages: [],
                    },
                ],
            },
            {
                id: 'char-t',
                configuredIndex: 3,
                detail: {
                    chaId: 'char-t',
                    name: 'Trashed',
                    type: 'character',
                    chatPage: 0,
                    trashTime: 1234,
                },
                conversations: [],
            },
        ],
    }
}

interface Step {
    name: string
    mutate(model: Model): ContentChangeKey[]
    /** The pass must give up and let the caller reproject. */
    expectFallback?: boolean
}

function characterKeys(character: ModelCharacter): ContentChangeKey[] {
    return [
        { kind: 'character', key1: character.id, key2: '' },
        ...character.conversations.map((conversation) => ({
            kind: 'conversation',
            key1: character.id,
            key2: conversation.id,
        })),
    ]
}

/// Each step declares exactly the locators the native triggers record for it.
const STEPS: Step[] = [
    {
        name: 'root setting change',
        mutate(model) {
            model.root.theme = 'dark'
            return [{ kind: 'root', key1: '', key2: '' }]
        },
    },
    {
        name: 'root module asset owner head',
        mutate(model) {
            model.root.modules = [{ id: 'module-one', assets: [['a', 'assets/a', 'bin']] }]
            return [
                { kind: 'owner', key1: 'root-module-assets', key2: '0' },
                { kind: 'root', key1: '', key2: '' },
            ]
        },
    },
    {
        name: 'preset rename',
        mutate(model) {
            model.presets[0] = { ...model.presets[0], name: 'Renamed' }
            return [
                { kind: 'preset', key1: '0', key2: '' },
                { kind: 'preset', key1: '1', key2: '' },
            ]
        },
    },
    {
        name: 'active preset switch',
        mutate(model) {
            model.root.botPresetsId = 1
            return [{ kind: 'root', key1: '', key2: '' }]
        },
    },
    {
        name: 'character detail edit',
        mutate(model) {
            const character = characterOf(model, 'char-b')!
            character.detail = { ...character.detail, name: 'Beta Renamed', image: 'beta.png' }
            return [{ kind: 'character', key1: 'char-b', key2: '' }]
        },
    },
    {
        name: 'character additional asset owner head',
        mutate(model) {
            const character = characterOf(model, 'char-b')!
            character.detail = {
                ...character.detail,
                additionalAssets: [['extra', 'assets/extra', 'bin']],
            }
            return [
                { kind: 'character', key1: 'char-b', key2: '' },
                { kind: 'owner', key1: 'character-additional-assets', key2: 'char-b' },
            ]
        },
    },
    {
        name: 'character addition',
        mutate(model) {
            const added: ModelCharacter = {
                id: 'char-new',
                configuredIndex: 4,
                detail: {
                    chaId: 'char-new',
                    name: 'New',
                    type: 'character',
                    chatPage: 0,
                    lastInteraction: 40,
                },
                conversations: [
                    {
                        id: 'conv-n1',
                        name: 'N One',
                        configuredIndex: 0,
                        recentAt: 40,
                        messages: [],
                    },
                ],
            }
            model.characters.push(added)
            return characterKeys(added)
        },
    },
    {
        name: 'conversation addition',
        mutate(model) {
            const character = characterOf(model, 'char-a')!
            character.conversations.push({
                id: 'conv-a3',
                name: 'A Three',
                configuredIndex: 2,
                recentAt: 12,
                messages: [],
            })
            return [
                { kind: 'character', key1: 'char-a', key2: '' },
                { kind: 'conversation', key1: 'char-a', key2: 'conv-a3' },
            ]
        },
    },
    {
        name: 'message edit in the selected conversation',
        mutate(model) {
            const conversation = characterOf(model, 'char-a')!.conversations.find(
                (candidate) => candidate.id === 'conv-a2',
            )!
            conversation.messages.push({ role: 'char', data: 'reply', chatId: 'm3' })
            conversation.recentAt = 50
            return [
                { kind: 'character', key1: 'char-a', key2: '' },
                { kind: 'conversation', key1: 'char-a', key2: 'conv-a2' },
            ]
        },
    },
    {
        name: 'message edit reported without its character locator',
        mutate(model) {
            const conversation = characterOf(model, 'char-a')!.conversations.find(
                (candidate) => candidate.id === 'conv-a1',
            )!
            conversation.messages.push({ role: 'char', data: 'later', chatId: 'm4' })
            conversation.recentAt = 60
            return [{ kind: 'conversation', key1: 'char-a', key2: 'conv-a1' }]
        },
    },
    {
        name: 'conversation deletion',
        mutate(model) {
            const character = characterOf(model, 'char-a')!
            character.conversations = character.conversations.filter(
                (candidate) => candidate.id !== 'conv-a3',
            )
            return [
                { kind: 'character', key1: 'char-a', key2: '' },
                { kind: 'conversation', key1: 'char-a', key2: 'conv-a3' },
            ]
        },
    },
    {
        name: 'plugin values written by two owners under one key',
        mutate() {
            return [
                { kind: 'plugin', key1: 'alpha', key2: 'shared' },
                { kind: 'plugin', key1: 'beta', key2: 'shared' },
            ]
        },
    },
    {
        name: 'asset and inlay reference changes',
        mutate() {
            return [
                { kind: 'asset', key1: 'asset-one', key2: '' },
                { kind: 'inlay', key1: 'inlay-one', key2: '' },
            ]
        },
    },
    {
        name: 'character trashed',
        mutate(model) {
            const character = characterOf(model, 'char-new')!
            character.detail = { ...character.detail, trashTime: 999 }
            return [{ kind: 'character', key1: 'char-new', key2: '' }]
        },
    },
    {
        name: 'character reorder',
        mutate(model) {
            characterOf(model, 'char-a')!.configuredIndex = 1
            characterOf(model, 'char-b')!.configuredIndex = 0
            return [
                { kind: 'character', key1: 'char-a', key2: '' },
                { kind: 'character', key1: 'char-b', key2: '' },
            ]
        },
    },
    {
        name: 'character archived',
        mutate(model) {
            const character = characterOf(model, 'char-b')!
            const messageCount = character.conversations.reduce(
                (total, conversation) => total + conversation.messages.length,
                0,
            )
            const keys = characterKeys(character)
            character.archived = {
                archivedAt: 1_700_000_000_000,
                conversationCount: character.conversations.length,
                messageCount,
            }
            character.conversations = []
            character.detail = {
                chaId: character.id,
                name: character.detail.name,
                type: character.detail.type,
            }
            return keys
        },
    },
    {
        name: 'group member reference changes while a member is archived',
        mutate(model) {
            const group = characterOf(model, 'group-g')!
            group.detail = { ...group.detail, characters: ['char-a'] }
            return [{ kind: 'character', key1: 'group-g', key2: '' }]
        },
    },
    {
        name: 'character unarchived',
        mutate(model) {
            const character = characterOf(model, 'char-b')!
            character.archived = undefined
            character.detail = {
                chaId: 'char-b',
                name: 'Beta Renamed',
                type: 'character',
                chatPage: 0,
                image: 'beta.png',
                lastInteraction: 20,
                additionalAssets: [['extra', 'assets/extra', 'bin']],
            }
            character.conversations = [
                {
                    id: 'conv-b1',
                    name: 'B One',
                    configuredIndex: 0,
                    recentAt: 20,
                    messages: [],
                },
            ]
            return characterKeys(character)
        },
    },
    {
        name: 'character deletion',
        mutate(model) {
            const character = characterOf(model, 'char-new')!
            model.characters = model.characters.filter(
                (candidate) => candidate.id !== 'char-new',
            )
            return characterKeys(character)
        },
    },
    {
        name: 'an unhandled locator kind',
        mutate(model) {
            model.root.theme = 'sepia'
            return [
                { kind: 'root', key1: '', key2: '' },
                { kind: 'cold', key1: 'anything', key2: '' },
            ]
        },
        expectFallback: true,
    },
    {
        name: 'a full replacement locator',
        mutate(model) {
            model.root.theme = 'contrast'
            return [{ kind: 'full', key1: '', key2: '' }]
        },
        expectFallback: true,
    },
]

const ALTERNATE_ORDER = [
    0, 4, 8, 2, 6, 15, 9, 1, 12, 17, 3, 7, 10, 5, 13, 11, 16, 14, 18, 19, 20,
]

interface Variant {
    name: string
    options(): Pick<
        TargetedWorkingSetInvalidationOptions,
        'selectedCharacterId' | 'selectedConversationId' | 'activeCharacterIds'
    >
}

const VARIANTS: Variant[] = [
    {
        name: 'an edited character selected',
        options: () => ({
            selectedCharacterId: 'char-a',
            selectedConversationId: 'conv-a2',
            activeCharacterIds: new Set<string>(),
        }),
    },
    {
        name: 'nothing selected',
        options: () => ({
            selectedCharacterId: null,
            selectedConversationId: null,
            activeCharacterIds: new Set<string>(),
        }),
    },
    {
        name: 'a group selected with resident members',
        options: () => ({
            selectedCharacterId: 'group-g',
            selectedConversationId: 'conv-g1',
            activeCharacterIds: new Set<string>(),
        }),
    },
    {
        name: 'another character selected while one stays resident',
        options: () => ({
            selectedCharacterId: 'char-b',
            selectedConversationId: 'conv-b1',
            activeCharacterIds: new Set(['char-a']),
        }),
    },
]

async function runCatalogue(order: readonly number[], variant: Variant): Promise<void> {
    const model = newModel()
    const options = variant.options()
    let carried: Database = await projectPinnedScalableWorkingSet(createReader(model), options)
    for (const index of order) {
        const step = STEPS[index]
        const keys = step.mutate(model)
        model.revision += 1
        const full = await projectPinnedScalableWorkingSet(createReader(model), options)
        const targeted = await applyTargetedWorkingSetInvalidation(
            carried,
            keys,
            createReader(model),
            { ...options, deferredConversation: null },
        )
        if (step.expectFallback) {
            expect(targeted, `${variant.name}: ${step.name} must fall back`).toBeNull()
            carried = full
            continue
        }
        expect(targeted, `${variant.name}: ${step.name} must apply`).not.toBeNull()
        expect(
            liftHiddenState(targeted!.database),
            `${variant.name}: ${step.name}`,
        ).toEqual(liftHiddenState(full))
        carried = targeted!.database
    }
}

describe('targeted invalidation equals a full reprojection', () => {
    it('refreshes a nonresident changed ID without reading unchanged character or preset bodies', async () => {
        const model = newModel()
        const options = {
            selectedCharacterId: 'char-a', selectedConversationId: 'conv-a2',
            activeCharacterIds: new Set<string>(),
        }
        const previous = await projectPinnedScalableWorkingSet(createReader(model), options)
        characterOf(model, 'char-b')!.detail.name = 'Updated summary'
        model.revision++
        const reader = createReader(model)
        const summary = vi.spyOn(reader, 'readCharacterSummary')
        const body = vi.spyOn(reader, 'readCharacter')
        const conversations = vi.spyOn(reader, 'queryConversations')
        const messages = vi.spyOn(reader, 'readConversation')
        const root = vi.spyOn(reader, 'readRoot')
        const presets = vi.spyOn(reader, 'queryPresets')
        const preset = vi.spyOn(reader, 'readPreset')
        const result = await applyTargetedWorkingSetInvalidation(previous, [], reader, {
            ...options,
            changeSet: {
                root: false, presets: false, pluginStorage: false, wholeLibrary: false,
                characterIds: ['char-b'], conversations: [],
            },
        })

        expect(result!.database.characters.find((value) => value.chaId === 'char-b')?.name)
            .toBe('Updated summary')
        expect(result!.database.characters[0]).toBe(previous.characters[0])
        expect(summary.mock.calls).toEqual([['char-b']])
        for (const read of [body, conversations, messages, root, presets, preset]) {
            expect(read).not.toHaveBeenCalled()
        }
        expect(result!.database.botPresets[0]).toBe(previous.botPresets[0])
        expect(getCatalogPresetMetadata(result!.database.botPresets)?.catalogRevision).toBe(2)
        expect(getCatalogPresetMetadata(previous.botPresets)?.catalogRevision).toBe(1)
    })

    it('reprojects only the root when a committed change set contains no character or preset changes', async () => {
        const model = newModel()
        const options = {
            selectedCharacterId: 'char-a', selectedConversationId: 'conv-a2',
            activeCharacterIds: new Set<string>(),
        }
        const previous = await projectPinnedScalableWorkingSet(createReader(model), options)
        model.root.username = 'Committed root'
        model.revision++
        const reader = createReader(model)
        const root = vi.spyOn(reader, 'readRoot')
        const summary = vi.spyOn(reader, 'readCharacterSummary')
        const body = vi.spyOn(reader, 'readCharacter')
        const presets = vi.spyOn(reader, 'queryPresets')
        const result = await applyTargetedWorkingSetInvalidation(previous, [], reader, {
            ...options,
            changeSet: {
                root: true, presets: false, pluginStorage: false, wholeLibrary: false,
                characterIds: [], conversations: [],
            },
        })

        expect(result!.database.username).toBe('Committed root')
        expect(root).toHaveBeenCalledOnce()
        expect(summary).not.toHaveBeenCalled()
        expect(body).not.toHaveBeenCalled()
        expect(presets).not.toHaveBeenCalled()
        for (let index = 0; index < previous.characters.length; index++) {
            expect(result!.database.characters[index]).toBe(previous.characters[index])
        }
    })

    for (const variant of VARIANTS) {
        it(`applies the change catalogue in order with ${variant.name}`, async () => {
            await runCatalogue(STEPS.map((_, index) => index), variant)
        })

        it(`applies the change catalogue interleaved with ${variant.name}`, async () => {
            await runCatalogue(ALTERNATE_ORDER, variant)
        })
    }

    it('gives up on a window wider than the targeted limit', async () => {
        const model = newModel()
        const options = {
            selectedCharacterId: 'char-a',
            selectedConversationId: 'conv-a2',
            activeCharacterIds: new Set<string>(),
        }
        const previous = await projectPinnedScalableWorkingSet(createReader(model), options)
        const keys: ContentChangeKey[] = Array.from(
            { length: TARGETED_INVALIDATION_KEY_LIMIT + 1 },
            (_, index) => ({ kind: 'character', key1: `char-${index}`, key2: '' }),
        )
        expect(
            await applyTargetedWorkingSetInvalidation(previous, keys, createReader(model), options),
        ).toBeNull()
    })

    it('gives up on a working set that is not a projection', async () => {
        const model = newModel()
        const options = {
            selectedCharacterId: null,
            selectedConversationId: null,
            activeCharacterIds: new Set<string>(),
        }
        const previous = await projectPinnedScalableWorkingSet(createReader(model), options)
        const complete = { ...previous, botPresets: [{ name: 'Plain' }] } as unknown as Database
        expect(
            await applyTargetedWorkingSetInvalidation(
                complete,
                [{ kind: 'root', key1: '', key2: '' }],
                createReader(model),
                options,
            ),
        ).toBeNull()
    })

    it('drops the host cache of every owner a plugin locator names', async () => {
        const model = newModel()
        const options = {
            selectedCharacterId: null,
            selectedConversationId: null,
            activeCharacterIds: new Set<string>(),
        }
        const previous = await projectPinnedScalableWorkingSet(createReader(model), options)
        const onPluginStorageChanged = vi.fn()
        await applyTargetedWorkingSetInvalidation(
            previous,
            [
                { kind: 'plugin', key1: 'alpha', key2: 'shared' },
                { kind: 'plugin', key1: 'beta', key2: 'shared' },
                { kind: 'asset', key1: 'asset-one', key2: '' },
            ],
            createReader(model),
            { ...options, onPluginStorageChanged },
        )
        expect(onPluginStorageChanged.mock.calls).toEqual([
            ['alpha', 'shared'],
            ['beta', 'shared'],
        ])
    })
})

describe('a generating conversation holds its remote change', () => {
    const options = {
        selectedCharacterId: 'char-a',
        selectedConversationId: 'conv-a2',
        activeCharacterIds: new Set<string>(),
    }

    it('keeps the working-set conversation and reports the change as deferred', async () => {
        const model = newModel()
        const previous = await projectPinnedScalableWorkingSet(createReader(model), options)
        const conversation = characterOf(model, 'char-a')!.conversations.find(
            (candidate) => candidate.id === 'conv-a2',
        )!
        conversation.messages = [{ role: 'char', data: 'remote', chatId: 'remote-1' }]
        model.revision += 1
        const targeted = await applyTargetedWorkingSetInvalidation(
            previous,
            [
                { kind: 'character', key1: 'char-a', key2: '' },
                { kind: 'conversation', key1: 'char-a', key2: 'conv-a2' },
            ],
            createReader(model),
            {
                ...options,
                deferredConversation: { characterId: 'char-a', conversationId: 'conv-a2' },
            },
        )
        expect(targeted?.deferred).toBe(true)
        const selected = targeted!.database.characters.find(
            (candidate) => candidate.chaId === 'char-a',
        )!
        expect(selected.chats[1].message).toEqual([
            { role: 'user', data: 'second', chatId: 'm2' },
        ])
    })

    it('applies a change to any other conversation of the same character', async () => {
        const model = newModel()
        const previous = await projectPinnedScalableWorkingSet(createReader(model), options)
        const conversation = characterOf(model, 'char-a')!.conversations.find(
            (candidate) => candidate.id === 'conv-a1',
        )!
        conversation.name = 'A One Renamed'
        model.revision += 1
        const targeted = await applyTargetedWorkingSetInvalidation(
            previous,
            [
                { kind: 'character', key1: 'char-a', key2: '' },
                { kind: 'conversation', key1: 'char-a', key2: 'conv-a1' },
            ],
            createReader(model),
            {
                ...options,
                deferredConversation: { characterId: 'char-a', conversationId: 'conv-a2' },
            },
        )
        expect(targeted?.deferred).toBe(false)
        const selected = targeted!.database.characters.find(
            (candidate) => candidate.chaId === 'char-a',
        )!
        expect(selected.chats[0].name).toBe('A One Renamed')
    })
})
