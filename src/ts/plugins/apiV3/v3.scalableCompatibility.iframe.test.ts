import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { Chat } from '../../storage/database.svelte'
import type { PluginCompleteCharacter } from '../pluginDatabaseAccess'
import type { RisuPlugin } from '../plugins.svelte'
import { dispatchChatOutputListeners } from '../pluginChatOutputListeners'

const fixture = vi.hoisted(() => {
    const requestedPermissions: string[] = []
    const listeners = new Set<any>()
    const releaseRevisionLease = vi.fn()
    const projectScalable = vi.fn()
    const scopedAccess = {
        getFullObjectSnapshotStream: async (target: { characterIndex?: number; chatIndex?: number }, context: unknown) => {
            const access = fixture.scopedAccess
            const value = await (target.characterIndex === undefined
                ? access.getCurrentCharacter(context)
                : target.chatIndex === undefined
                    ? access.getCharacterFromIndex(target.characterIndex, context)
                    : access.getChatFromIndex(target.characterIndex, target.chatIndex, context))
            if (!value) return value
            const isConversation = target.chatIndex !== undefined
            return {
                __type: 'IFRAME_OBJECT_STREAM',
                select: isConversation ? 'conversation' : 'character',
                value: new ReadableStream({
                    start(controller) {
                        controller.enqueue({ type: 'arrayStart', key: 'characters' })
                        const { chats, ...detail } = isConversation ? { chats: [value] } : value
                        controller.enqueue({ type: 'characterStart', key: 'characters', value: detail })
                        for (const chat of chats) {
                            const { message, ...metadata } = chat
                            controller.enqueue({ type: 'conversationStart', key: 'characters', value: metadata })
                            for (const value of message) controller.enqueue({ type: 'message', key: 'characters', value })
                        }
                        controller.close()
                    },
                }),
            }
        },
        getCurrentCharacter: vi.fn(),
        getCharacterFromIndex: vi.fn(),
        getChatFromIndex: vi.fn(),
        setCurrentCharacter: vi.fn(),
        setCharacterToIndex: vi.fn(),
        setChatToIndex: vi.fn(),
    }
    const database = {
        characters: [],
        plugins: [],
    } as any
    return {
        requestedPermissions,
        listeners,
        releaseRevisionLease,
        projectScalable,
        scopedAccess,
        database,
        permissionGetItem: vi.fn(async (key: string) => {
            const permission = key.match(/_([^_]+)_lastGrantTime$/)?.[1]
            if (permission) requestedPermissions.push(permission)
            return null
        }),
    }
})

const ownedStorageStub = {
    getItem: vi.fn(), setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(),
    key: vi.fn(), keys: vi.fn(), length: vi.fn(), snapshot: vi.fn(async () => ({})),
    mutate: vi.fn(),
}

vi.mock('../plugins.svelte', () => {
    const unrelated = vi.fn()
    const oldApis = new Proxy({
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
        pluginV2: {
            providers: new Map(),
            providerOptions: new Map(),
            chatOutput: fixture.listeners,
        },
    }
})
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => fixture.database }))
vi.mock('../pluginSafeClass', () => ({ SafeLocalPluginStorage: class {}, SafeLocalStorage: class {}, tagWhitelist: [] }))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: { get db() { return fixture.database } },
    selectedCharID: {
        subscribe(run: (value: number) => void) { run(0); return () => undefined },
    },
    additionalChatMenu: [], additionalFloatingActionButtons: [], additionalHamburgerMenu: [],
    additionalSettingsMenu: [], bodyIntercepterStore: [], chatPanelStore: [],
}))
vi.mock('src/ts/alert', () => ({
    alertConfirm: vi.fn(async () => true), alertError: vi.fn(), alertNormal: vi.fn(),
}))
vi.mock('src/ts/util', () => ({ sleep: vi.fn(async () => undefined) }))
vi.mock('src/lang', () => ({ language: {
    fetchLogConsent: '{}', getFullDatabaseConsent: '{}', mainDomAccessConsent: '{}',
    replacerPermissionConsent: '{}', providerPermissionConsent: '{}', sendChatConsent: '{}',
    inlayPermissionConsent: '{}',
} }))
vi.mock('src/ts/globalApi.svelte', () => ({
    checkCharOrder: vi.fn(), forageStorage: {}, getFetchLogs: vi.fn(),
}))
vi.mock('src/ts/gui/colorscheme', () => ({
    changeColorScheme: vi.fn(), updateColorScheme: vi.fn(), updateTextThemeAndCSS: vi.fn(),
}))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('src/ts/process/mcp/pluginmcp', () => ({
    registerMCPModule: vi.fn(), unregisterMCPModule: vi.fn(),
}))
vi.mock('src/ts/process/files/inlays', () => ({ getInlayAsset: vi.fn() }))
vi.mock('src/ts/translator/translator', () => ({ getLLMCache: vi.fn(), searchLLMCache: vi.fn() }))
vi.mock('src/ts/parser/parser.svelte', () => ({ hasher: vi.fn(async () => 'hash') }))
vi.mock('localforage', () => ({ default: { createInstance: () => ({
    getItem: fixture.permissionGetItem,
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
vi.mock('../pluginDatabaseAccess', async (importOriginal) => {
    const actual = await importOriginal<typeof import('../pluginDatabaseAccess')>()
    return {
        ...actual,
        createProductionPluginDatabaseAccess: vi.fn(() => fixture.scopedAccess),
        linkPluginQueryAbortSignals: vi.fn(),
    }
})

import { loadV3Plugins } from './v3.svelte'

const trashCharacter = {
    type: 'character',
    chaId: 'trash-character',
    name: 'Trash',
    trashTime: 1,
    chatPage: 0,
    chats: [{
        id: 'trash-chat',
        name: 'Trash chat',
        note: '',
        localLore: [],
        message: [{ role: 'char', data: 'before output' }],
    }],
} as PluginCompleteCharacter
const finalLiveChat = {
    ...structuredClone(trashCharacter.chats[0]),
    note: 'final live output',
    message: [
        ...trashCharacter.chats[0].message,
        { role: 'char', data: 'generated output' },
    ],
} as Chat
const finalLiveCharacter = {
    ...structuredClone(trashCharacter),
    chats: [finalLiveChat],
} as PluginCompleteCharacter

function plugin(name: string, script: string): RisuPlugin {
    return {
        name,
        script,
        arguments: {},
        realArg: {},
        version: '3.0',
        customLink: [],
        argMeta: {},
        enabled: true,
    }
}

async function executeIframeSrcdoc(frame: HTMLIFrameElement): Promise<void> {
    const child = frame.contentWindow!
    const childRealm = child as any
    Object.defineProperty(child, 'ImageBitmap', {
        configurable: true,
        value: globalThis.ImageBitmap,
    })
    vi.spyOn(window, 'postMessage').mockImplementation((data) => {
        window.dispatchEvent(new MessageEvent('message', { data, source: child }))
    })
    vi.spyOn(child.parent, 'postMessage').mockImplementation((data) => {
        window.dispatchEvent(new MessageEvent('message', { data, source: child }))
    })
    vi.spyOn(child, 'postMessage').mockImplementation((data, _origin, transfer?: Transferable[]) => {
        child.dispatchEvent(new childRealm.MessageEvent('message', { data, source: window, ports: transfer ?? [] }))
    })
    const source = frame.srcdoc.match(/<script nonce="[^"]+">([\s\S]*)<\/script>/)?.[1]
    if (!source) throw new Error('Sandbox guest script was not found')
    await childRealm.eval(source)
}

function currentFrame(): HTMLIFrameElement {
    const frame = document.querySelector<HTMLIFrameElement>('iframe[data-risu-plugin-frame]')
    if (!frame) throw new Error('Plugin iframe was not created')
    return frame
}

beforeEach(() => {
    vi.clearAllMocks()
    vi.stubGlobal('ImageBitmap', class {})
    fixture.listeners.clear()
    fixture.requestedPermissions.length = 0
    fixture.database.characters = [structuredClone(trashCharacter)]
    fixture.scopedAccess.getCharacterFromIndex.mockImplementation(async (index: number) =>
        index === 1 ? structuredClone(trashCharacter) : null)
    fixture.scopedAccess.getChatFromIndex.mockImplementation(async (
        characterIndex: number,
        chatIndex: number,
    ) => characterIndex === 1 && chatIndex === 0
        ? structuredClone(trashCharacter.chats[0])
        : null)
    fixture.scopedAccess.setCurrentCharacter.mockResolvedValue(undefined)
    fixture.scopedAccess.setCharacterToIndex.mockResolvedValue(undefined)
    fixture.scopedAccess.setChatToIndex.mockResolvedValue(undefined)
    fixture.projectScalable.mockImplementation(async () => ({
        char: structuredClone(finalLiveCharacter),
        chat: structuredClone(finalLiveChat),
    }))
    vi.spyOn(console, 'log').mockImplementation(() => undefined)
})

afterEach(async () => {
    await loadV3Plugins([])
    vi.restoreAllMocks()
})

describe('Plugin v3 scalable synthetic iframe compatibility', () => {
    it('routes guest full-object calls and projected output callbacks through scalable v3', async () => {
        const acceptancePlugin = plugin('iframe-acceptance', `
window.acceptance = (async () => {
    const character = await risuai.getCharacterFromIndex(1)
    const chat = await risuai.getChatFromIndex(1, 0)
    character.name = 'iframe character update'
    chat.localLore = [{ key: 'iframe', content: 'saved' }]
    await risuai.setCharacterToIndex(1, character)
    await risuai.setChatToIndex(1, 0, chat)
    await risuai.addRisuChatListener('output', async (event) => {
        window.listenerEvent = event
        const latest = await risuai.getChatFromIndex(
            event.characterIndex,
            event.chatIndex,
        )
        latest.note = 'listener persisted'
        await risuai.setChatToIndex(event.characterIndex, event.chatIndex, latest)
    })
    return {
        characterId: character.chaId,
        chatId: chat.id,
        invalidCharacter: await risuai.getCharacterFromIndex(999),
        invalidChat: await risuai.getChatFromIndex(999, 999),
    }
})()
`)
        fixture.database.plugins = [acceptancePlugin]

        await loadV3Plugins([acceptancePlugin])
        const frame = currentFrame()
        await executeIframeSrcdoc(frame)
        const result = await (frame.contentWindow as any).acceptance

        await dispatchChatOutputListeners({
            listeners: fixture.listeners,
            char: finalLiveCharacter,
            chat: finalLiveChat,
            characterIndex: 1,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable: fixture.projectScalable,
            onError: (error) => { throw error },
        })

        expect(result).toEqual({
            characterId: 'trash-character',
            chatId: 'trash-chat',
            invalidCharacter: null,
            invalidChat: null,
        })
        expect(fixture.scopedAccess.setCharacterToIndex).toHaveBeenCalledOnce()
        expect(fixture.scopedAccess.setCharacterToIndex).toHaveBeenCalledWith(
            1,
            expect.objectContaining({ name: 'iframe character update' }),
            expect.objectContaining({ signal: expect.any(AbortSignal) }),
        )
        expect(fixture.scopedAccess.setChatToIndex).toHaveBeenCalledTimes(2)
        expect(fixture.scopedAccess.setChatToIndex).toHaveBeenNthCalledWith(
            1,
            1,
            0,
            expect.objectContaining({
                localLore: [{ key: 'iframe', content: 'saved' }],
            }),
            expect.objectContaining({ signal: expect.any(AbortSignal) }),
        )
        expect(fixture.scopedAccess.setChatToIndex).toHaveBeenNthCalledWith(
            2,
            1,
            0,
            expect.objectContaining({ note: 'listener persisted' }),
            expect.objectContaining({ signal: expect.any(AbortSignal) }),
        )
        expect((frame.contentWindow as any).listenerEvent.chat.note).toBe('final live output')
        expect(fixture.projectScalable).toHaveBeenCalledOnce()
        expect(fixture.requestedPermissions).toEqual(['replacer'])
    })

    it('aborts an in-flight guest getter and removes its listener on unload', async () => {
        const unloadPlugin = plugin('iframe-unload', `
window.ready = (async () => {
    window.listenerCalls = 0
    await risuai.addRisuChatListener('output', async () => {
        window.listenerCalls += 1
    })
    window.pendingGetter = risuai.getCharacterFromIndex(1)
    return true
})()
`)
        fixture.database.plugins = [unloadPlugin]
        fixture.scopedAccess.getCharacterFromIndex.mockImplementation((
            _index: number,
            context: { signal: AbortSignal },
        ) => new Promise((_resolve, reject) => {
            context.signal.addEventListener('abort', () => {
                fixture.releaseRevisionLease()
                reject(context.signal.reason)
            }, { once: true })
        }))

        await loadV3Plugins([unloadPlugin])
        const frame = currentFrame()
        const child = frame.contentWindow as any
        await executeIframeSrcdoc(frame)
        await child.ready
        await vi.waitFor(() => expect(fixture.scopedAccess.getCharacterFromIndex).toHaveBeenCalledOnce())
        const pendingGetter = child.pendingGetter as Promise<unknown>

        const unloading = loadV3Plugins([])
        await expect(pendingGetter).rejects.toThrow(/abort/i)
        await unloading

        expect(fixture.releaseRevisionLease).toHaveBeenCalledOnce()
        expect(fixture.listeners).toHaveLength(0)
        await dispatchChatOutputListeners({
            listeners: fixture.listeners,
            char: finalLiveCharacter,
            chat: finalLiveChat,
            characterIndex: 1,
            chatIndex: 0,
            messageIndex: 1,
            projectScalable: fixture.projectScalable,
            onError: (error) => { throw error },
        })
        expect(child.listenerCalls).toBe(0)
        expect(fixture.projectScalable).not.toHaveBeenCalled()
    })
})
