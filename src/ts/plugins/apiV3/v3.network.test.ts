import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    api: null as any,
    nativeFetch: vi.fn(),
    risuFetch: vi.fn(),
    database: null as any,
    selectedId: 0,
    session: null as any,
    selectedTarget: null as any,
    acquireCompleteConversation: vi.fn(),
    processSendChat: vi.fn(),
    doingChat: false,
}))

vi.mock('../plugins.svelte', () => {
    const oldApis = new Proxy(
        { nativeFetch: mocks.nativeFetch, risuFetch: mocks.risuFetch },
        {
            get: (target, key) => target[key] ?? vi.fn(),
        },
    )
    return {
        allowedDbKeys: [],
        applyPreparedPluginDatabaseUpdate: vi.fn(),
        customProviderStore: {
            subscribe: (run: (value: string[]) => void) => {
                run([])
                return () => undefined
            },
            set: vi.fn(),
        },
        getV2PluginAPIs: () => oldApis,
        handlePluginInstallViaPlugin: vi.fn(),
        pluginCompatibility: { profile: 'scalable' },
        pluginStorageStore: {
            snapshot: () => [],
            mutate: vi.fn(),
            invalidate: vi.fn(),
            getItem: vi.fn(),
            setItem: vi.fn(),
            removeItem: vi.fn(),
            clear: vi.fn(),
            key: vi.fn(),
            keys: vi.fn(),
            length: vi.fn(),
        },
        pluginV2: { providers: new Map(), providerOptions: new Map() },
    }
})
vi.mock('./factory', () => ({
    SandboxHost: class {
        constructor(api: unknown) {
            mocks.api = api
        }
        run() {}
        terminate() {}
    },
}))
vi.mock('src/ts/storage/database.svelte', () => ({
    getDatabase: () => mocks.database,
}))
vi.mock('../pluginSafeClass', () => ({
    SafeLocalPluginStorage: class {},
    tagWhitelist: [],
}))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: {
        get db() {
            return mocks.database
        },
        set db(value) {
            mocks.database = value
        },
    },
    selectedCharID: {
        subscribe(run: (value: number) => void) {
            run(mocks.selectedId)
            return () => undefined
        },
    },
    additionalChatMenu: [],
    additionalFloatingActionButtons: [],
    additionalHamburgerMenu: [],
    additionalSettingsMenu: [],
    bodyIntercepterStore: [],
    chatPanelStore: [],
}))
vi.mock('src/ts/alert', () => ({
    alertConfirm: vi.fn(async () => true),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
}))
vi.mock('src/ts/util', () => ({ sleep: vi.fn(async () => undefined) }))
vi.mock('src/lang', () => ({
    language: {
        fetchLogConsent: '{}',
        getFullDatabaseConsent: '{}',
        mainDomAccessConsent: '{}',
        replacerPermissionConsent: '{}',
        providerPermissionConsent: '{}',
        sendChatConsent: '{}',
        inlayPermissionConsent: '{}',
    },
}))
vi.mock('src/ts/globalApi.svelte', () => ({
    checkCharOrder: vi.fn(),
    forageStorage: {},
    getFetchLogs: vi.fn(),
}))
vi.mock('src/ts/gui/colorscheme', () => ({
    changeColorScheme: vi.fn(),
    updateColorScheme: vi.fn(),
    updateTextThemeAndCSS: vi.fn(),
}))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))
vi.mock('src/ts/process/mcp/pluginmcp', () => ({
    registerMCPModule: vi.fn(),
    unregisterMCPModule: vi.fn(),
}))
vi.mock('src/ts/process/files/inlays', () => ({ getInlayAsset: vi.fn() }))
vi.mock('src/ts/translator/translator', () => ({
    getLLMCache: vi.fn(),
    searchLLMCache: vi.fn(),
}))
vi.mock('src/ts/parser/parser.svelte', () => ({
    hasher: vi.fn(async () => 'hash'),
}))
vi.mock('localforage', () => ({
    default: { createInstance: () => ({ getItem: vi.fn(), setItem: vi.fn() }) },
}))
vi.mock('src/ts/process/index.svelte', () => ({
    sendChat: mocks.processSendChat,
    doingChat: {
        subscribe(run: (value: boolean) => void) {
            run(mocks.doingChat)
            return () => undefined
        },
        set(value: boolean) {
            mocks.doingChat = value
        },
    },
}))
vi.mock('src/ts/model/modellist', () => ({
    getModelInfo: () => ({ id: 'test-model' }),
}))
vi.mock('src/ts/process/request/request', () => ({
    requestChatDataMain: vi.fn(),
}))
vi.mock('src/ts/process/modules', () => ({ getModuleLorebooks: vi.fn() }))
vi.mock('src/ts/process/ttsHooks', () => ({
    registerTTSPreprocessor: vi.fn(),
    unregisterTTSPreprocessor: vi.fn(),
    registerTTSPostprocessor: vi.fn(),
    unregisterTTSPostprocessor: vi.fn(),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    flushPendingData: vi.fn(),
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

describe('Plugin v3 network access', () => {
    beforeEach(async () => {
        vi.clearAllMocks()
        mocks.database = { plugins: [], characters: [] }
        await executePluginV3({
            name: `network-fixture-${crypto.randomUUID()}`,
            script: '',
        } as any)
    })

    it.each(['nativeFetch', 'risuFetch'])(
        '%s allows unrelated hosts containing blocked domain text elsewhere',
        async (apiName) => {
            const options = { method: 'GET' }
            const response = { status: 200 }
            mocks[apiName].mockResolvedValue(response)
            // These are only arguments to a mock transport. No external requests occur.
            for (const url of [
                'https://api.example.invalid:8443/path',
                'http://localhost:11434/api/chat',
                'https://api.example.invalid/?callback=https://risuai.xyz/',
                'https://api.example.invalid/path/risuai.net/file',
                'https://api.example.invalid/#https://sionyw.com/',
                'https://risuai.xyz@api.example.invalid/path',
                'https://risuai.xyz.example.invalid/',
                'https://risuai.net.example.invalid/',
                'https://sionyw.com.example.invalid/',
                'https://notrisuai.xyz/',
                'https://notrisuai.net/',
                'https://notsionyw.com/',
                '//api.example.invalid/path/risuai.xyz',
                '/path/risuai.net',
                './path?callback=https://sionyw.com/',
            ]) {
                await expect(mocks.api[apiName](url, options)).resolves.toBe(
                    response,
                )
                expect(mocks[apiName]).toHaveBeenLastCalledWith(url, options)
            }
        },
    )

    it.each(['nativeFetch', 'risuFetch'])(
        '%s blocks configured hosts and their subdomains before transport',
        (apiName) => {
            for (const domain of ['risuai.xyz', 'risuai.net', 'sionyw.com']) {
                for (const url of [
                    `https://${domain}/`,
                    `http://${domain}:8080/path`,
                    `https://api.${domain}/path`,
                    `https://nested.api.${domain}:8443/`,
                    `https://${domain.toUpperCase()}/`,
                    `https://${domain}./`,
                    `https://api.${domain.toUpperCase()}.:8443/`,
                    `https://allowed.example.invalid@${domain}/`,
                    `https://${domain.replace('i', '%69')}/`,
                    `//${domain}/`,
                ]) {
                    expect(() =>
                        mocks.api[apiName](url, { method: 'GET' }),
                    ).toThrow(
                        `Requests to ${domain} are blocked for security reasons.`,
                    )
                }
            }
            expect(mocks[apiName]).not.toHaveBeenCalled()
        },
    )

    it.each(['nativeFetch', 'risuFetch'])(
        '%s checks the resolved host for a relative browser URL',
        (apiName) => {
            const baseURI = vi
                .spyOn(document, 'baseURI', 'get')
                .mockReturnValue('https://risuai.xyz/app/')
            try {
                expect(() => mocks.api[apiName]('./api')).toThrow(
                    'Requests to risuai.xyz are blocked for security reasons.',
                )
                expect(mocks[apiName]).not.toHaveBeenCalled()
            } finally {
                baseURI.mockRestore()
            }
        },
    )
})
