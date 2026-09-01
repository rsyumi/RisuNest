import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { Database } from '../../storage/database.svelte'

const fixture = vi.hoisted(() => ({
    api: null as Record<string, (...args: any[]) => any> | null,
    selectedIndex: 0,
    profile: 'maximum-compatibility' as 'scalable-v3' | 'maximum-compatibility',
    invalidations: 0,
    databaseAccessDependencies: null as null | { getSelectedCharacterId(): string | null },
    pluginPermissionReads: vi.fn(),
    listeners: new Set<Function>(),
    scopedAccess: {
        getCurrentCharacter: vi.fn(),
        getCharacterFromIndex: vi.fn(),
        getChatFromIndex: vi.fn(),
        setCurrentCharacter: vi.fn(),
        setCharacterToIndex: vi.fn(),
        setChatToIndex: vi.fn(),
    },
    database: {
        characters: [
            {
                type: 'character', chaId: 'active', name: 'Active', chatPage: 0,
                chats: [{ id: 'active-chat', name: 'Live', message: [{ role: 'char', data: 'a' }] }],
            },
            {
                type: 'character', chaId: 'trashed', name: 'Trash', trashTime: 1, chatPage: 0,
                chats: [{ id: 'trash-chat', name: 'Trash chat', message: [{ role: 'user', data: 'b' }] }],
            },
        ],
        plugins: [{ name: 'contract-plugin', script: '' }],
    } as unknown as Database,
}))

vi.mock('../plugins.svelte', () => {
    const unrelated = vi.fn()
    const oldApis = new Proxy({
        getChar: () => {
            const character = fixture.database.characters[fixture.selectedIndex]
            return character === undefined ? undefined : structuredClone(character)
        },
        setChar: (character: Database['characters'][number]) => {
            if (fixture.database.characters[fixture.selectedIndex]) {
                fixture.database.characters[fixture.selectedIndex] = character
            }
        },
        addRisuChatListener: (_mode: string, listener: Function) => fixture.listeners.add(listener),
        removeRisuChatListener: (_mode: string, listener: Function) => fixture.listeners.delete(listener),
        safeLocalStorage: {
            getItem: unrelated,
            setItem: unrelated,
            removeItem: unrelated,
            clear: unrelated,
            key: unrelated,
            keys: unrelated,
            length: unrelated,
        },
    }, { get: (target, property) => Reflect.get(target, property) ?? unrelated })
    return {
        allowedDbKeys: [],
        applyPreparedPluginDatabaseUpdate: vi.fn(),
        customProviderStore: {
            subscribe(run: (value: string[]) => void) { run([]); return () => undefined },
            set: vi.fn(),
        },
        getV2PluginAPIs: () => oldApis,
        handlePluginInstallViaPlugin: vi.fn(),
        pluginCompatibility: {
            get profile() { return fixture.profile },
            get allowsEviction() { return fixture.profile === 'scalable-v3' },
        },
        pluginStorageStore: {
            snapshot: vi.fn(async () => []), mutate: unrelated, invalidate: unrelated,
            getItem: unrelated, setItem: unrelated, removeItem: unrelated,
            clear: unrelated, key: unrelated, keys: unrelated, length: unrelated,
        },
        pluginV2: { providers: new Map(), providerOptions: new Map() },
    }
})
vi.mock('./factory', () => ({
    SandboxHost: class {
        constructor(api: Record<string, (...args: any[]) => any>) { fixture.api = api }
        run() {}
        terminate() {}
    },
}))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => fixture.database }))
vi.mock('../pluginSafeClass', () => ({ SafeLocalPluginStorage: class {}, tagWhitelist: [] }))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: {
        get db() { return fixture.database },
        set db(value) { fixture.database = value },
    },
    selectedCharID: {
        subscribe(run: (value: number) => void) {
            run(fixture.selectedIndex)
            return () => undefined
        },
    },
    additionalChatMenu: [], additionalFloatingActionButtons: [], additionalHamburgerMenu: [],
    additionalSettingsMenu: [], bodyIntercepterStore: [], chatPanelStore: [],
}))
vi.mock('src/ts/alert', () => ({ alertConfirm: vi.fn(async () => true), alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('src/ts/util', () => ({ sleep: vi.fn(async () => undefined) }))
vi.mock('src/lang', () => ({ language: {
    fetchLogConsent: '{}', getFullDatabaseConsent: '{}', mainDomAccessConsent: '{}',
    replacerPermissionConsent: '{}', providerPermissionConsent: '{}', sendChatConsent: '{}',
    inlayPermissionConsent: '{}',
} }))
vi.mock('src/ts/globalApi.svelte', () => ({ checkCharOrder: vi.fn(), forageStorage: {}, getFetchLogs: vi.fn() }))
vi.mock('src/ts/gui/colorscheme', () => ({ changeColorScheme: vi.fn(), updateColorScheme: vi.fn(), updateTextThemeAndCSS: vi.fn() }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('src/ts/process/mcp/pluginmcp', () => ({ registerMCPModule: vi.fn(), unregisterMCPModule: vi.fn() }))
vi.mock('src/ts/process/files/inlays', () => ({ getInlayAsset: vi.fn() }))
vi.mock('src/ts/translator/translator', () => ({ getLLMCache: vi.fn(), searchLLMCache: vi.fn() }))
vi.mock('src/ts/parser/parser.svelte', () => ({ hasher: vi.fn(async () => 'hash') }))
vi.mock('localforage', () => ({ default: { createInstance: () => ({
    getItem: fixture.pluginPermissionReads,
    setItem: vi.fn(),
}) } }))
vi.mock('src/ts/process/index.svelte', () => ({
    sendChat: vi.fn(),
    doingChat: { subscribe(run: (value: boolean) => void) { run(false); return () => undefined } },
}))
vi.mock('src/ts/model/modellist', () => ({ getModelInfo: () => ({ id: 'test-model' }) }))
vi.mock('src/ts/process/request/request', () => ({ requestChatDataMain: vi.fn() }))
vi.mock('src/ts/process/modules', () => ({ getModuleLorebooks: vi.fn() }))
vi.mock('src/ts/process/ttsHooks', () => ({
    registerTTSPreprocessor: vi.fn(), unregisterTTSPreprocessor: vi.fn(),
    registerTTSPostprocessor: vi.fn(), unregisterTTSPostprocessor: vi.fn(),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireCompleteConversation: vi.fn(),
    captureSelectedConversationTarget: vi.fn(() => null),
    flushPendingData: vi.fn(),
    getActiveConversationSession: vi.fn(() => null),
    getPersistentNavigationGeneration: vi.fn(() => 0),
    invalidateActiveConversationSession: vi.fn(),
    materializePersistentDatabaseSnapshotWithRevision: vi.fn(),
    refreshSelectedConversationAfterReplacement: vi.fn(),
    replacePersistentCompleteCharacter: vi.fn(),
    replacePersistentConversation: vi.fn(),
    replacePersistentDatabase: vi.fn(),
}))
vi.mock('../pluginCompatibility', () => ({
    assertPluginFullObjectCompatibility: vi.fn(),
    runPluginFullObjectReplacement: vi.fn((
        _profile: string,
        _operation: string,
        affectsActiveConversation: boolean,
        replacement: () => unknown,
        invalidate: () => void,
    ) => {
        const result = replacement()
        if (affectsActiveConversation) {
            fixture.invalidations++
            invalidate()
        }
        return result
    }),
}))
vi.mock('../pluginDatabaseAccess', () => ({
    createProductionPluginDatabaseAccess: vi.fn((dependencies) => {
        fixture.databaseAccessDependencies = dependencies
        return fixture.scopedAccess
    }),
    linkPluginQueryAbortSignals: vi.fn(),
}))

import { executePluginV3 } from './v3.svelte'

function resetDatabase(): void {
    fixture.database = {
        characters: [
            {
                type: 'character', chaId: 'active', name: 'Active', chatPage: 0,
                chats: [{ id: 'active-chat', name: 'Live', message: [{ role: 'char', data: 'a' }] }],
            },
            {
                type: 'character', chaId: 'trashed', name: 'Trash', trashTime: 1, chatPage: 0,
                chats: [{ id: 'trash-chat', name: 'Trash chat', message: [{ role: 'user', data: 'b' }] }],
            },
        ],
        plugins: [{ name: 'contract-plugin', script: '' }],
    } as unknown as Database
}

describe('Plugin v3 maximum full-object compatibility', () => {
    beforeEach(async () => {
        vi.clearAllMocks()
        resetDatabase()
        fixture.api = null
        fixture.selectedIndex = 0
        fixture.profile = 'maximum-compatibility'
        fixture.invalidations = 0
        fixture.listeners.clear()
        fixture.scopedAccess.getCurrentCharacter.mockReset()
        fixture.scopedAccess.getCharacterFromIndex.mockReset()
        fixture.scopedAccess.getChatFromIndex.mockReset()
        fixture.scopedAccess.setCurrentCharacter.mockReset()
        fixture.scopedAccess.setCharacterToIndex.mockReset()
        fixture.scopedAccess.setChatToIndex.mockReset()
        await executePluginV3({
            name: `contract-plugin-${crypto.randomUUID()}`,
            script: '',
        } as any)
    })

    it('keeps maximum getters detached and preserves undefined and null results', async () => {
        const api = fixture.api!
        const current = await api.getCharacter()
        const indexed = await api.getCharacterFromIndex(1)
        const chat = await api.getChatFromIndex(1, 0)

        expect(current).toEqual(fixture.database.characters[0])
        expect(indexed).toEqual(fixture.database.characters[1])
        expect(chat).toEqual(fixture.database.characters[1].chats[0])
        current.name = 'detached'
        chat.name = 'detached chat'
        expect(fixture.database.characters[0].name).toBe('Active')
        expect(fixture.database.characters[1].chats[0].name).toBe('Trash chat')
        expect(await api.getCharacterFromIndex(-1)).toBeNull()
        expect(await api.getChatFromIndex(99, 0)).toBeNull()

        fixture.selectedIndex = -1
        expect(await api.getCharacter()).toBeUndefined()
    })

    it('deeply detaches current and indexed character and chat getters', async () => {
        const api = fixture.api!
        const current = await api.getCharacter()
        const indexed = await api.getCharacterFromIndex(1)
        const indexedChat = await api.getChatFromIndex(1, 0)

        current.chats[0].name = 'mutated current chat'
        current.chats[0].message[0].data = 'mutated current message'
        indexed.chats[0].message[0].data = 'mutated indexed message'
        indexedChat.message[0].data = 'mutated direct chat message'

        expect(fixture.database.characters[0].chats[0]).toMatchObject({
            name: 'Live',
            message: [{ role: 'char', data: 'a' }],
        })
        expect(fixture.database.characters[1].chats[0].message).toEqual([
            { role: 'user', data: 'b' },
        ])
    })

    it('keeps maximum ID replacement and invalid-index no-op behavior', async () => {
        const api = fixture.api!
        const replacementCharacter = structuredClone(fixture.database.characters[1])
        replacementCharacter.chaId = 'replacement-id'
        await api.setCharacterToIndex(1, replacementCharacter)
        expect(fixture.database.characters[1].chaId).toBe('replacement-id')

        const replacementChat = structuredClone(fixture.database.characters[0].chats[0])
        replacementChat.id = 'replacement-chat-id'
        await api.setChatToIndex(0, 0, replacementChat)
        expect(fixture.database.characters[0].chats[0].id).toBe('replacement-chat-id')
        expect(fixture.invalidations).toBe(1)

        const before = structuredClone(fixture.database)
        expect(await api.setCharacterToIndex(99, replacementCharacter)).toBeUndefined()
        expect(await api.setChatToIndex(99, 99, replacementChat)).toBeUndefined()
        expect(fixture.database).toEqual(before)
    })

    it('invalidates only maximum replacements affecting the active conversation', async () => {
        const api = fixture.api!
        await api.setCharacter(structuredClone(fixture.database.characters[0]))
        expect(fixture.invalidations).toBe(1)

        await api.setCharacterToIndex(1, structuredClone(fixture.database.characters[1]))
        await api.setChatToIndex(0, 99, structuredClone(fixture.database.characters[0].chats[0]))
        expect(fixture.invalidations).toBe(1)

        fixture.database.characters[0].chats.push({
            id: 'other-chat', name: 'Other', message: [],
        } as any)
        await api.setChatToIndex(0, 1, structuredClone(fixture.database.characters[0].chats[1]))
        expect(fixture.invalidations).toBe(1)

        fixture.selectedIndex = 1
        await api.setCharacterToIndex(1, structuredClone(fixture.database.characters[1]))
        expect(fixture.invalidations).toBe(2)
    })

    it('fully replaces ordinary and nested fields through current and indexed setters', async () => {
        const api = fixture.api!
        const currentReplacement = structuredClone(fixture.database.characters[0])
        currentReplacement.name = 'Current replacement'
        currentReplacement.chats[0].name = 'Current nested replacement'
        currentReplacement.chats[0].message = [{ role: 'user', data: 'current body' }]

        await api.setCharacter(currentReplacement)
        expect(fixture.database.characters[0]).toEqual(currentReplacement)

        const indexedReplacement = structuredClone(fixture.database.characters[1])
        indexedReplacement.name = 'Indexed replacement'
        indexedReplacement.chats[0].name = 'Indexed nested replacement'
        indexedReplacement.chats[0].message = [{ role: 'char', data: 'indexed body' }]
        await api.setCharacterToIndex(1, indexedReplacement)
        expect(fixture.database.characters[1]).toEqual(indexedReplacement)

        const chatReplacement = structuredClone(fixture.database.characters[0].chats[0])
        chatReplacement.name = 'Chat replacement'
        chatReplacement.message = [{ role: 'char', data: 'chat body' }]
        await api.setChatToIndex(0, 0, chatReplacement)
        expect(fixture.database.characters[0].chats[0]).toEqual(chatReplacement)
    })

    it('keeps valid-character invalid-chat access fulfilled and unchanged', async () => {
        const api = fixture.api!
        const before = structuredClone(fixture.database)
        const replacement = structuredClone(fixture.database.characters[0].chats[0])
        replacement.name = 'must not be installed'

        expect(await api.getChatFromIndex(0, 99)).toBeNull()
        expect(await api.setChatToIndex(0, 99, replacement)).toBeUndefined()
        expect(fixture.database).toEqual(before)
    })

    it('keeps getChar/setChar aliases aligned with getCharacter/setCharacter', async () => {
        const api = fixture.api!
        expect(await api.getChar()).toEqual(await api.getCharacter())

        const legacyReplacement = structuredClone(fixture.database.characters[0])
        legacyReplacement.name = 'Legacy alias replacement'
        legacyReplacement.chats[0].message[0].data = 'legacy nested replacement'
        await api.setChar(legacyReplacement)
        expect(fixture.database.characters[0]).toEqual(legacyReplacement)

        const namedReplacement = structuredClone(fixture.database.characters[0])
        namedReplacement.name = 'Named alias replacement'
        namedReplacement.chats[0].message[0].data = 'named nested replacement'
        await api.setCharacter(namedReplacement)
        expect(fixture.database.characters[0]).toEqual(namedReplacement)
    })

    it('routes scalable getters through scoped access without changing the maximum oracle', async () => {
        fixture.profile = 'scalable-v3'
        const active = structuredClone(fixture.database.characters[0])
        const trashed = structuredClone(fixture.database.characters[1])
        fixture.scopedAccess.getCurrentCharacter.mockResolvedValue(active)
        fixture.scopedAccess.getCharacterFromIndex.mockResolvedValue(trashed)
        fixture.scopedAccess.getChatFromIndex.mockResolvedValue(trashed.chats[0])
        const api = fixture.api!

        await expect(api.getCharacter()).resolves.toMatchObject({ chaId: 'active' })
        await expect(api.getCharacterFromIndex(1)).resolves.toMatchObject({ chaId: 'trashed' })
        await expect(api.getChatFromIndex(1, 0)).resolves.toMatchObject({ id: 'trash-chat' })

        expect(fixture.profile).toBe('scalable-v3')
        expect(fixture.pluginPermissionReads).not.toHaveBeenCalledWith(
            expect.stringContaining('_db'),
        )
        const { pluginCompatibility } = await import('../plugins.svelte')
        expect(pluginCompatibility.allowsEviction).toBe(true)

        expect(fixture.scopedAccess.getCurrentCharacter.mock.calls[0][0]).toMatchObject({
            pluginName: expect.stringContaining('contract-plugin-'),
            signal: expect.any(AbortSignal),
        })
        expect(fixture.scopedAccess.getCharacterFromIndex).toHaveBeenCalledWith(
            1,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.scopedAccess.getChatFromIndex).toHaveBeenCalledWith(
            1,
            0,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
    })

    it('resolves a selected character independently of conversation validity', async () => {
        fixture.profile = 'scalable-v3'
        fixture.scopedAccess.getCurrentCharacter.mockResolvedValue(
            structuredClone(fixture.database.characters[0]),
        )
        await fixture.api!.getCharacter()
        fixture.database.characters[0].chats = []
        fixture.database.characters[0].chatPage = 99

        expect(fixture.databaseAccessDependencies?.getSelectedCharacterId()).toBe('active')
    })

    it('routes scalable setters through scoped access while maximum ID replacement remains intact', async () => {
        fixture.profile = 'scalable-v3'
        const api = fixture.api!
        const character = structuredClone(fixture.database.characters[1])
        const chat = structuredClone(fixture.database.characters[1].chats[0])

        await api.setChar(character)
        await api.setCharacter(character)
        await api.setCharacterToIndex(1, character)
        await api.setChatToIndex(1, 0, chat)

        expect(fixture.scopedAccess.setCurrentCharacter).toHaveBeenCalledTimes(2)
        expect(fixture.scopedAccess.setCurrentCharacter).toHaveBeenCalledWith(
            character,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.scopedAccess.setCharacterToIndex).toHaveBeenCalledWith(
            1,
            character,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.scopedAccess.setChatToIndex).toHaveBeenCalledWith(
            1,
            0,
            chat,
            expect.objectContaining({ pluginName: expect.stringContaining('contract-plugin-') }),
        )
        expect(fixture.database.characters[1].name).toBe('Trash')
        expect(fixture.profile).toBe('scalable-v3')
        expect(fixture.pluginPermissionReads).not.toHaveBeenCalledWith(
            expect.stringContaining('_db'),
        )
        const { pluginCompatibility } = await import('../plugins.svelte')
        expect(pluginCompatibility.allowsEviction).toBe(true)
    })
})
