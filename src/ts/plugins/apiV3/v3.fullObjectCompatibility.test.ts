import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { Database } from '../../storage/database.svelte'

const fixture = vi.hoisted(() => ({
    api: null as Record<string, (...args: any[]) => any> | null,
    selectedIndex: 0,
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

const ownedStorageStub = {
    getItem: vi.fn(), setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(),
    key: vi.fn(), keys: vi.fn(), length: vi.fn(), snapshot: vi.fn(async () => ({})),
    mutate: vi.fn(),
}

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
        pluginStorageStore: {
            forOwner: () => ownedStorageStub,
            ownerOf: () => 'test-plugin',
            invalidateOwner: vi.fn(),
            synchronizeCommittedMutation: vi.fn(),
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
vi.mock('../pluginSafeClass', () => ({ SafeLocalPluginStorage: class {}, SafeLocalStorage: class {}, tagWhitelist: [] }))
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
    flushPendingDataLocally: vi.fn(),
    assertPersistentMutationAllowed: vi.fn(),
    getPersistentStorageAuthorityEpoch: () => 0,
    getActiveConversationSession: vi.fn(() => null),
    getPersistentNavigationGeneration: vi.fn(() => 0),
    invalidateActiveConversationSession: vi.fn(),
    materializePersistentDatabaseSnapshotWithRevision: vi.fn(),
    refreshSelectedConversationAfterReplacement: vi.fn(),
    replacePersistentCompleteCharacter: vi.fn(),
    replacePersistentConversation: vi.fn(),
    replacePersistentDatabase: vi.fn(),
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

describe('Plugin v3 full-object access routing', () => {
    beforeEach(async () => {
        vi.clearAllMocks()
        resetDatabase()
        fixture.api = null
        fixture.selectedIndex = 0
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

    it('routes getters through scoped access', async () => {
        const active = structuredClone(fixture.database.characters[0])
        const trashed = structuredClone(fixture.database.characters[1])
        fixture.scopedAccess.getCurrentCharacter.mockResolvedValue(active)
        fixture.scopedAccess.getCharacterFromIndex.mockResolvedValue(trashed)
        fixture.scopedAccess.getChatFromIndex.mockResolvedValue(trashed.chats[0])
        const api = fixture.api!

        await expect(api.getCharacter()).resolves.toMatchObject({ chaId: 'active' })
        await expect(api.getCharacterFromIndex(1)).resolves.toMatchObject({ chaId: 'trashed' })
        await expect(api.getChatFromIndex(1, 0)).resolves.toMatchObject({ id: 'trash-chat' })

        expect(fixture.pluginPermissionReads).not.toHaveBeenCalledWith(
            expect.stringContaining('_db'),
        )

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
        fixture.scopedAccess.getCurrentCharacter.mockResolvedValue(
            structuredClone(fixture.database.characters[0]),
        )
        await fixture.api!.getCharacter()
        fixture.database.characters[0].chats = []
        fixture.database.characters[0].chatPage = 99

        expect(fixture.databaseAccessDependencies?.getSelectedCharacterId()).toBe('active')
    })

    it('routes setters through scoped access', async () => {
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
        expect(fixture.pluginPermissionReads).not.toHaveBeenCalledWith(
            expect.stringContaining('_db'),
        )
    })
})
