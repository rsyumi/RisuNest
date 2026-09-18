import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

import { ActiveConversationSession } from '../../storage/activeConversationSession'
import { doingChat as generationDoingChat } from '../../process/generationState'

const mocks = vi.hoisted(() => ({
    api: null as any,
    database: null as any,
    selectedId: 0,
    authorityEpoch: 0,
    session: null as any,
    selectedTarget: null as any,
    acquireCompleteConversation: vi.fn(),
    processSendChat: vi.fn(),
    doingChat: false,
}))

const ownedStorageStub = {
    getItem: vi.fn(), setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(),
    key: vi.fn(), keys: vi.fn(), length: vi.fn(), snapshot: vi.fn(async () => ({})),
    mutate: vi.fn(),
}

vi.mock('../plugins.svelte', () => {
    const oldApis = new Proxy({}, { get: () => vi.fn() })
    return {
        allowedDbKeys: [],
        applyPreparedPluginDatabaseUpdate: vi.fn(),
        customProviderStore: { subscribe: (run: (value: string[]) => void) => { run([]); return () => undefined }, set: vi.fn() },
        getV2PluginAPIs: () => oldApis,
        handlePluginInstallViaPlugin: vi.fn(),
        pluginCompatibility: { profile: 'scalable' },
        pluginStorageStore: {
            forOwner: () => ownedStorageStub,
            ownerOf: () => 'test-plugin',
            invalidateOwner: vi.fn(),
            synchronizeCommittedMutation: vi.fn(),
            snapshot: () => [], mutate: vi.fn(), invalidate: vi.fn(), getItem: vi.fn(),
            setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(), key: vi.fn(),
            keys: vi.fn(), length: vi.fn(),
        },
        pluginV2: { providers: new Map(), providerOptions: new Map() },
    }
})
vi.mock('./factory', () => ({
    SandboxHost: class {
        constructor(api: unknown) { mocks.api = api }
        run() {}
        terminate() {}
    },
}))
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => mocks.database }))
vi.mock('../pluginSafeClass', () => ({ SafeLocalPluginStorage: class {}, SafeLocalStorage: class {}, tagWhitelist: [] }))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: { get db() { return mocks.database }, set db(value) { mocks.database = value } },
    selectedCharID: {
        subscribe(run: (value: number) => void) {
            run(mocks.selectedId)
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
    sendChat: mocks.processSendChat,
    doingChat: {
        subscribe(run: (value: boolean) => void) { run(mocks.doingChat); return () => undefined },
        set(value: boolean) { mocks.doingChat = value },
    },
}))
vi.mock('src/ts/model/modellist', () => ({ getModelInfo: () => ({ id: 'test-model' }) }))
vi.mock('src/ts/process/request/request', () => ({ requestChatDataMain: vi.fn() }))
vi.mock('src/ts/process/modules', () => ({ getModuleLorebooks: vi.fn() }))
vi.mock('src/ts/process/ttsHooks', () => ({
    registerTTSPreprocessor: vi.fn(), unregisterTTSPreprocessor: vi.fn(),
    registerTTSPostprocessor: vi.fn(), unregisterTTSPostprocessor: vi.fn(),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    flushPendingDataLocally: vi.fn(),
    assertPersistentMutationAllowed: (epoch = mocks.authorityEpoch) => {
        if (epoch !== mocks.authorityEpoch) throw new Error('Persistent mutation fenced')
    },
    getPersistentStorageAuthorityEpoch: () => mocks.authorityEpoch,
    getActiveConversationSession: () => mocks.session,
    getPersistentNavigationGeneration: () => 0,
    getPersistentDataRuntime: vi.fn(),
    getPersistentDataStore: vi.fn(),
    invalidateActiveConversationSession: vi.fn(),
    materializePersistentDatabaseSnapshotWithRevision: vi.fn(),
    replacePersistentDatabase: vi.fn(),
}))
vi.mock('../pluginCompatibility', () => ({
    assertPluginFullObjectCompatibility: vi.fn(),
    preparePluginFullObjectCallbackRegistration: vi.fn(),
    runPluginFullObjectReplacement: vi.fn(),
}))
vi.mock('../pluginDatabaseAccess', () => ({
    createProductionPluginDatabaseAccess: vi.fn(() => ({})),
    linkPluginQueryAbortSignals: vi.fn(),
}))

import { executePluginV3 } from './v3.svelte'

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => { resolve = resolvePromise })
    return { promise, resolve }
}

describe('Plugin v3 sendChat complete mutation gateway', () => {
    beforeEach(async () => {
        vi.clearAllMocks()
        generationDoingChat.set(false)
        mocks.api = null
        mocks.selectedId = 0
        mocks.doingChat = false
        const chat = { id: 'chat-a', message: [{ role: 'char', data: 'before' }] }
        const character = { type: 'character', chaId: 'character-a', chatPage: 0, chats: [chat] }
        mocks.database = { aiModel: 'test-model', plugins: [{ name: 'lease-plugin', script: '' }], characters: [character] }
        mocks.session = new ActiveConversationSession({
            characterId: character.chaId,
            conversationId: chat.id,
            conversation: chat as any,
            storeRevision: 1,
        })
        mocks.selectedTarget = null
        mocks.acquireCompleteConversation.mockReset()
        mocks.processSendChat.mockReset()
        await executePluginV3({ name: `lease-plugin-${crypto.randomUUID()}`, script: '' } as any)
    })

    it('preserves direct no-session send behavior when no selected target exists', async () => {
        const chat = mocks.database.characters[0].chats[0]
        mocks.processSendChat.mockResolvedValue(true)

        await expect(mocks.api.sendChat('hello')).resolves.toBe(true)

        expect(chat.message.map((message: any) => message.data)).toEqual(['before', 'hello'])
        expect(mocks.acquireCompleteConversation).not.toHaveBeenCalled()
        expect(mocks.processSendChat).toHaveBeenCalledOnce()
    })

    it('shares the public reservation before async promotion so concurrent calls cannot both append', async () => {
        const chat = mocks.database.characters[0].chats[0]
        const target = { characterId: 'character-a', conversationId: 'chat-a' }
        mocks.selectedTarget = target
        const firstPromotion = deferred<any>()
        const secondReachedPromotion = new Error('second call reached promotion')
        mocks.acquireCompleteConversation
            .mockReturnValueOnce(firstPromotion.promise)
            .mockRejectedValueOnce(secondReachedPromotion)
        mocks.processSendChat.mockResolvedValue(true)
        let firstReleases = 0

        const first = mocks.api.sendChat('first')
        while (mocks.acquireCompleteConversation.mock.calls.length === 0) await Promise.resolve()
        const secondOutcome = await mocks.api.sendChat('second').catch((error: unknown) => error)

        expect(get(generationDoingChat)).toBe(true)

        firstPromotion.resolve({
            session: mocks.session,
            target,
            release() { firstReleases += 1 },
        })
        await expect(first).resolves.toBe(true)

        expect(secondOutcome).toBeInstanceOf(Error)
        expect((secondOutcome as Error).message).toBe('A chat is already in progress')
        expect(mocks.acquireCompleteConversation).toHaveBeenCalledTimes(1)
        expect(chat.message.map((message: any) => message.data)).toEqual(['before', 'first'])
        expect(firstReleases).toBe(1)
    })

    it('returns false and removes only its appended message when generation declines', async () => {
        const chat = mocks.database.characters[0].chats[0]
        mocks.processSendChat.mockResolvedValue(false)

        await expect(mocks.api.sendChat('declined')).resolves.toBe(false)

        expect(chat.message.map((message: any) => message.data)).toEqual(['before'])
    })

    it('removes only its appended message and releases once when generation throws', async () => {
        const chat = mocks.database.characters[0].chats[0]
        const target = { characterId: 'character-a', conversationId: 'chat-a' }
        mocks.selectedTarget = target
        const failure = new Error('generation failed')
        mocks.processSendChat.mockRejectedValue(failure)
        let releaseCount = 0
        mocks.acquireCompleteConversation.mockResolvedValue({
            session: mocks.session,
            target,
            release() { releaseCount += 1 },
        })

        await expect(mocks.api.sendChat('failed')).rejects.toBe(failure)

        expect(chat.message.map((message: any) => message.data)).toEqual(['before'])
        expect(releaseCount).toBe(1)
    })

    it('promotes before append and holds one exact lease until delegated generation settles', async () => {
        const chat = mocks.database.characters[0].chats[0]
        const target = { characterId: 'character-a', conversationId: 'chat-a' }
        mocks.selectedTarget = target
        const promotion = deferred<any>()
        mocks.acquireCompleteConversation.mockReturnValue(promotion.promise)
        const generation = deferred<boolean>()
        mocks.processSendChat.mockReturnValue(generation.promise)
        let releaseCount = 0

        const sending = mocks.api.sendChat('hello')
        while (mocks.acquireCompleteConversation.mock.calls.length === 0) await Promise.resolve()

        expect(mocks.acquireCompleteConversation).toHaveBeenCalledOnce()
        expect(chat.message.map((message: any) => message.data)).toEqual(['before'])
        expect(mocks.processSendChat).not.toHaveBeenCalled()

        const pin = mocks.session.acquirePin('compatibility')
        promotion.resolve({
            session: mocks.session,
            target,
            release() {
                releaseCount += 1
                pin.release()
            },
        })
        while (mocks.processSendChat.mock.calls.length === 0) await Promise.resolve()

        expect(chat.message.map((message: any) => message.data)).toEqual(['before', 'hello'])
        expect(mocks.session.pinCount('compatibility')).toBe(1)

        generation.resolve(true)
        await expect(sending).resolves.toBe(true)
        expect(releaseCount).toBe(1)
        expect(mocks.session.pinCount('compatibility')).toBe(0)
    })

    it('aborts before append when the promoted lease does not own the selected chat', async () => {
        const chat = mocks.database.characters[0].chats[0]
        const otherChat = { id: 'chat-b', message: [] }
        const otherSession = new ActiveConversationSession({
            characterId: 'character-a',
            conversationId: 'chat-b',
            conversation: otherChat as any,
            storeRevision: 1,
        })
        mocks.selectedTarget = { characterId: 'character-a', conversationId: 'chat-a' }
        let releaseCount = 0
        mocks.acquireCompleteConversation.mockResolvedValue({
            session: otherSession,
            target: mocks.selectedTarget,
            release() { releaseCount += 1 },
        })

        await expect(mocks.api.sendChat('hello')).resolves.toBe(false)

        expect(chat.message.map((message: any) => message.data)).toEqual(['before'])
        expect(mocks.processSendChat).not.toHaveBeenCalled()
        expect(releaseCount).toBe(1)
    })
})
