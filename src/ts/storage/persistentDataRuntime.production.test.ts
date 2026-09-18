import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

vi.mock('../parser/parser.svelte', () => ({
    assetRegex: /$^/,
    hasher: vi.fn(async () => 'hash'),
    parseMarkdownSafe: (value: string) => value,
    ParseMarkdown: vi.fn(async (value: string) => value),
    risuChatParser: (value: string) => value,
}))
import { selectedCharID } from '../stores.svelte'
import { doingChat } from '../process/generationState'
import { getRuntimePerformanceBudgets } from '../runtimePerformanceProfile'
import {
    createPluginStorageStore,
    registerPluginStorageLifecycle,
} from '../plugins/pluginStorageStore'
import { getV2PluginAPIs } from '../plugins/plugins.svelte'
import type { Database } from './database.svelte'
import type { PersistentDataStore } from './persistentDataStore'
import { getDatabase, setDatabaseLite } from './database.svelte'
import {
    configurePersistentDataRuntime,
    createProductionStateAdapter,
    hydrateCurrentGroupMemberDetail,
} from './persistentDataRuntime.svelte'
import {
    createCatalogCharacterStub,
    isCatalogCharacterStub,
    projectCompleteScalableWorkingSet,
} from './workingSetCatalog'
import { workingSetResidency } from './workingSetResidency'
import { SaveCoordinator } from './saveCoordinator'

afterEach(() => {
    configurePersistentDataRuntime({ projectWorkingSet: undefined })
    workingSetResidency.clear()
    workingSetResidency.setEvictionAllowed(true)
    doingChat.set(false)
})

describe('production persistent working-set publication', () => {
    it('uses canonical captures for immediate production edits and emits a small delta', async () => {
        setDatabaseLite({
            username: 'Before',
            customBackground: 'x'.repeat(6 * 1024 * 1024),
            botPresets: [],
            plugins: [],
            characters: [
                {
                    type: 'character',
                    chaId: 'synthetic-large',
                    name: 'Synthetic',
                    chatPage: 0,
                    chats: [
                        {
                            id: 'synthetic-chat',
                            message: [{ role: 'char', data: 'x'.repeat(2 * 1024 * 1024) }],
                        },
                    ],
                },
            ],
            pluginCustomStorage: {},
        } as unknown as Database)
        selectedCharID.set(0)
        const adapter = createProductionStateAdapter()
        expect(adapter.canonicalCapture).toBeDefined()
        const commit = vi.fn(async () => ({ revision: 2 }))
        const coordinator = new SaveCoordinator({
            ...adapter,
            captureRoot: () => {
                throw new Error('Must use the production canonical capture')
            },
            store: { commit } as unknown as PersistentDataStore,
        })
        coordinator.initialize(1)
        getDatabase().username = 'Immediately changed'
        coordinator.markPersistentDataDirty(1)
        const parse = vi.spyOn(JSON, 'parse')
        try {
            await coordinator.flushPendingData('immediate-production-mutation')
            expect(parse.mock.calls.some(([value]) => value.length > 1024 * 1024)).toBe(false)
        } finally {
            parse.mockRestore()
        }
        expect(commit).toHaveBeenCalledExactlyOnceWith({
            expectedRevision: 1,
            rootMutations: [{ type: 'set', key: 'username', value: 'Immediately changed' }],
        })
        expect(JSON.stringify(commit.mock.calls[0]).length).toBeLessThan(256)
    })

    it('restores only affected activation entries, selection and residency', () => {
        const selected = {
            type: 'character',
            chaId: 'char-a',
            name: 'Selected',
            chats: [
                {
                    id: 'chat-a',
                    name: 'Chat',
                    note: '',
                    localLore: [],
                    message: [],
                },
            ],
            chatPage: 0,
        }
        const related = createCatalogCharacterStub({
            id: 'char-b',
            configuredIndex: 1,
            conversationCount: 2,
            name: 'Related',
            type: 'character',
            recentAt: 0,
            trashed: false,
        })
        const unaffected = {
            type: 'character',
            chaId: 'char-c',
            name: 'Unaffected',
            chats: [],
        }
        setDatabaseLite({
            botPresets: [],
            plugins: [],
            characters: [selected, related, unaffected],
        } as unknown as Database)
        selectedCharID.set(0)
        const residentSelected = getDatabase().characters[0]
        const residentRelated = getDatabase().characters[1]
        workingSetResidency.markCharacterHydrated('char-a')
        workingSetResidency.reconcileConversationResidency(residentSelected)
        workingSetResidency.markCharacterReleased('char-b')
        const restore = createProductionStateAdapter()
            .captureActivationRollback!(['char-a', 'char-b'])

        getDatabase().characters[0] = {
            type: 'character',
            chaId: 'char-a',
            name: 'Tentative selected',
            chats: [],
        } as any
        getDatabase().characters[1] = {
            type: 'character',
            chaId: 'char-b',
            name: 'Tentative related',
            chats: [],
        } as any
        getDatabase().characters[2].name = 'Concurrent unaffected edit'
        selectedCharID.set(1)
        workingSetResidency.markCharacterReleased('char-a')
        workingSetResidency.markCharacterHydrated('char-b')

        restore()

        expect(getDatabase().characters[0]).toBe(residentSelected)
        expect(getDatabase().characters[1]).toBe(residentRelated)
        expect(getDatabase().characters[2].name).toBe(
            'Concurrent unaffected edit',
        )
        expect(get(selectedCharID)).toBe(0)
        expect(workingSetResidency.isCharacterReleased('char-a')).toBe(false)
        expect(
            workingSetResidency.canReleaseConversation(
                residentSelected,
                'chat-a',
            ),
        ).toBe(false)
        expect(workingSetResidency.isCharacterReleased('char-b')).toBe(true)
    })

    it('hydrates only the restored catalog member while preserving the selected group', () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            name: 'Group',
            characters: ['member-a'],
            characterTalks: [1],
            characterActive: [true],
            chats: [{ id: 'group-chat', message: [] }],
            chatPage: 0,
        }
        const memberStub = createCatalogCharacterStub({
            id: 'member-b',
            configuredIndex: 1,
            conversationCount: 0,
            name: 'Beta',
            type: 'character',
            recentAt: 0,
            trashed: false,
        })
        const database = {
            botPresets: [],
            plugins: [],
            characters: [group, memberStub],
        } as unknown as Database
        setDatabaseLite(database)
        selectedCharID.set(0)
        const residentGroup = getDatabase().characters[0]
        const residentMemberStub = getDatabase().characters[1]
        const detail = {
            type: 'character',
            chaId: 'member-b',
            name: 'Beta',
            personality: 'Persistent personality',
            scenario: 'Persistent scenario',
        } as any

        expect(hydrateCurrentGroupMemberDetail('group-a', detail)).toBe(true)

        expect(getDatabase().characters[0]).toBe(residentGroup)
        expect(getDatabase().characters[1]).toMatchObject({
            personality: 'Persistent personality',
            scenario: 'Persistent scenario',
        })
        expect(isCatalogCharacterStub(getDatabase().characters[1])).toBe(false)
        expect(getDatabase().characters[1].chats).toBe(residentMemberStub.chats)
        expect(getDatabase().characters[get(selectedCharID)]).toBe(residentGroup)
    })

    it('leaves an already complete maximum-compatibility member unchanged', () => {
        const group = {
            type: 'group',
            chaId: 'group-a',
            characters: [],
            characterTalks: [],
            characterActive: [],
            chats: [],
        }
        const member = {
            type: 'character',
            chaId: 'member-b',
            personality: 'Complete personality',
            chats: [],
        }
        setDatabaseLite({
            botPresets: [],
            plugins: [{ enabled: true, version: '2.1' }],
            characters: [group, member],
        } as unknown as Database)
        selectedCharID.set(0)
        const residentMember = getDatabase().characters[1]

        expect(hydrateCurrentGroupMemberDetail('group-a', {
            type: 'character',
            chaId: 'member-b',
            personality: 'Replacement personality',
        } as any)).toBe(true)

        expect(getDatabase().characters[1]).toBe(residentMember)
        expect(getDatabase().characters[1].personality).toBe('Complete personality')
    })

    it('reports selected lifecycle policy, operation and viewport budget', () => {
        setDatabaseLite({
            botPresets: [],
            characters: [],
            plugins: [],
        } as unknown as Database)
        workingSetResidency.setEvictionAllowed(true)
        doingChat.set(false)
        const adapter = createProductionStateAdapter()
        const operationTransitions: boolean[] = []
        const unsubscribeOperation = adapter.subscribeConversationOperationActive?.(
            (active) => operationTransitions.push(active),
        )

        expect(adapter.canUseWindowedSelectedConversation?.()).toBe(true)
        expect(adapter.isConversationOperationActive?.()).toBe(false)
        expect(adapter.conversationViewportRowBudget).toBe(
            getRuntimePerformanceBudgets().chatMountedMessageBudget,
        )

        workingSetResidency.setEvictionAllowed(false)
        doingChat.set(true)

        expect(adapter.canUseWindowedSelectedConversation?.()).toBe(false)
        expect(adapter.isConversationOperationActive?.()).toBe(true)
        doingChat.set(false)
        expect(operationTransitions).toEqual([false, true, false])
        unsubscribeOperation?.()
    })

    it('clears old residency before the synchronous projector records the replacement', () => {
        const initial = {
            botPresetsId: 0,
            botPresets: [{ name: 'Active' }],
            characters: [{
                type: 'group',
                chaId: 'group-a',
                name: 'Group',
                characters: ['member-a'],
                characterTalks: [1],
                characterActive: [true],
                chats: [],
            }],
        } as unknown as Database
        const replacement = {
            ...initial,
            characters: [{
                type: 'character',
                chaId: 'member-a',
                name: 'Member',
                personality: 'resident detail',
                chats: [],
            }, initial.characters[0], {
                type: 'character',
                chaId: 'inactive',
                name: 'Inactive',
                personality: 'must be released',
                chats: [],
            }],
        } as unknown as Database
        setDatabaseLite(initial)
        selectedCharID.set(0)
        workingSetResidency.markCharacterReleased('stale')
        let oldResidencyClearedBeforeProjection = false
        configurePersistentDataRuntime({
            projectWorkingSet(database, selectedCharacterId, selectedConversationId, activeIds) {
                oldResidencyClearedBeforeProjection =
                    !workingSetResidency.isCharacterReleased('stale')
                const projected = projectCompleteScalableWorkingSet(
                    database,
                    selectedCharacterId,
                    2,
                    activeIds,
                    selectedConversationId,
                )
                for (const character of projected.characters) {
                    if (isCatalogCharacterStub(character)) {
                        workingSetResidency.markCharacterReleased(character.chaId)
                    }
                }
                return projected
            },
        })

        createProductionStateAdapter().replaceDatabase(
            replacement,
            new Set(['group-a', 'member-a']),
            true,
        )

        expect(oldResidencyClearedBeforeProjection).toBe(true)
        expect(workingSetResidency.isCharacterReleased('inactive')).toBe(true)
        expect(getDatabase().characters[0].personality).toBe('resident detail')
        expect(isCatalogCharacterStub(getDatabase().characters[2])).toBe(true)
    })

    it('preloads nested maximum plugin values from the installed plain database', async () => {
        const nestedValue = {
            list: [{ enabled: true }],
            settings: { mode: 'maximum' },
        }
        const storage = createPluginStorageStore({
            getStorageAuthorityEpoch: () => 0,
            assertPersistentMutationAllowed: vi.fn(),
            store: {
                open: async () => undefined,
                queryPluginStorage: async () => ({
                    revision: 1,
                    items: [{ owner: UNOWNED_PLUGIN_OWNER, key: 'nested', byteSize: 1 }],
                }),
                readPluginStorage: async () => ({ revision: 1, value: nestedValue }),
            } as unknown as PersistentDataStore,
            mutate: async () => undefined,
        })
        const unregister = registerPluginStorageLifecycle(storage)
        const complete = {
            botPresets: [],
            pluginCustomStorage: { nested: nestedValue },
            characters: [],
        } as unknown as Database
        try {
            createProductionStateAdapter().installCompleteDatabase!(complete)

            await expect(storage.forOwner(UNOWNED_PLUGIN_OWNER).keys()).resolves.toEqual(['nested'])
            await expect(storage.forOwner(UNOWNED_PLUGIN_OWNER).getItem('nested')).resolves.toEqual(nestedValue)
            expect(await storage.forOwner(UNOWNED_PLUGIN_OWNER).getItem('nested')).not.toBe(nestedValue)
        } finally {
            unregister()
        }
    })
})
