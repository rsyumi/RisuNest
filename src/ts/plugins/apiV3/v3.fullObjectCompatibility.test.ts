import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { Database } from '../../storage/database.types'

const fixture = vi.hoisted(() => ({
    api: null as Record<string, (...args: any[]) => any> | null,
    selectedIndex: 0,
    profile: 'maximum-compatibility' as const,
    invalidations: 0,
    listeners: new Set<Function>(),
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
            allowsEviction: false,
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
vi.mock('localforage', () => ({ default: { createInstance: () => ({ getItem: vi.fn(), setItem: vi.fn() }) } }))
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
    replacePersistentDatabase: vi.fn(),
}))
vi.mock('../pluginCompatibility', () => ({
    assertPluginFullObjectCompatibility: vi.fn(),
    preparePluginFullObjectCallbackRegistration: vi.fn(() => true),
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
    createProductionPluginDatabaseAccess: vi.fn(() => ({})),
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
        fixture.invalidations = 0
        fixture.listeners.clear()
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
})
