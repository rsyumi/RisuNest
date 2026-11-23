import { beforeEach, describe, expect, it, vi } from 'vitest'
import versionData from '../../../../version.json'

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

vi.mock('../plugins.svelte', async () => {
    const host = await import('./v3Host.testSupport')
    const apis = host.v2PluginApis({ nativeFetch: mocks.nativeFetch, risuFetch: mocks.risuFetch })
    return host.pluginsModule({
        getV2PluginAPIs: () => apis,
        pluginV2: { providers: new Map(), providerOptions: new Map() },
    })
})
vi.mock('./factory', async () => (await import('./v3Host.testSupport')).sandboxHostModule((api) => { mocks.api = api }))
vi.mock('src/ts/storage/database.svelte', async () => (await import('./v3Host.testSupport')).databaseModule({
    read: () => mocks.database,
    write: (database) => { mocks.database = database },
    selectedIndex: () => mocks.selectedId,
}))
vi.mock('../pluginSafeClass', async () => (await import('./v3Host.testSupport')).pluginSafeClassModule())
vi.mock('src/ts/stores.svelte', async () => (await import('./v3Host.testSupport')).storesModule({
    read: () => mocks.database,
    write: (database) => { mocks.database = database },
    selectedIndex: () => mocks.selectedId,
}))
vi.mock('src/ts/alert', async () => (await import('./v3Host.testSupport')).alertModule())
vi.mock('src/ts/util', async () => (await import('./v3Host.testSupport')).utilModule())
vi.mock('src/ts/globalApi.svelte', () => ({
    checkCharOrder: vi.fn(),
    forageStorage: {},
    getFetchLogs: vi.fn(),
}))
vi.mock('src/ts/gui/colorscheme', async () => (await import('./v3Host.testSupport')).colorschemeModule())
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('src/ts/process/mcp/pluginmcp', async () => (await import('./v3Host.testSupport')).pluginMcpModule())
vi.mock('src/ts/process/files/inlays', async () => (await import('./v3Host.testSupport')).inlaysModule())
vi.mock('src/ts/translator/translator', async () => (await import('./v3Host.testSupport')).translatorModule())
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('localforage', async () => (await import('../localforage.testSupport')).localforageModule())
vi.mock('src/ts/process/index.svelte', async () => (await import('./v3Host.testSupport')).processIndexModule({
    sendChat: mocks.processSendChat,
    doingChat: {
        subscribe(run: (value: boolean) => void) { run(mocks.doingChat); return () => undefined },
        set(value: boolean) { mocks.doingChat = value },
    },
}))
vi.mock('src/ts/model/modellist', async () => (await import('./v3Host.testSupport')).modellistModule())
vi.mock('src/ts/process/request/request', async () => (await import('./v3Host.testSupport')).requestModule())
vi.mock('src/ts/process/modules', async () => (await import('./v3Host.testSupport')).modulesModule())
vi.mock('src/ts/process/ttsHooks', async () => (await import('./v3Host.testSupport')).ttsHooksModule())
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => (await import('./v3Host.testSupport')).persistentRuntimeModule({
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    getActiveConversationSession: () => mocks.session,
    getPersistentDataRuntime: vi.fn(),
}))
vi.mock('../pluginDatabaseAccess', async () => (await import('./v3Host.testSupport')).pluginDatabaseAccessModule())

import { executePluginV3 } from './v3.svelte'

describe('Plugin v3 runtime information', () => {
    it('returns application identity through RPC without sharing mutable metadata', async () => {
        mocks.database = { plugins: [], characters: [] }
        await executePluginV3({
            name: `runtime-fixture-${crypto.randomUUID()}`,
            script: '',
        } as any)
        const { SandboxHost } = await vi.importActual<typeof import('./factory')>('./factory')
        const host = new SandboxHost(mocks.api)
        const frame = document.createElement('iframe')
        document.body.appendChild(frame)
        host.run(frame, '', 'runtime-fixture')
        const child = frame.contentWindow!
        const postMessage = vi.spyOn(child, 'postMessage').mockImplementation(() => {})

        try {
            for (const reqId of ['first-runtime-read', 'second-runtime-read']) {
                postMessage.mockClear()
                window.dispatchEvent(new MessageEvent('message', {
                    source: child,
                    data: { type: 'CALL_ROOT', reqId, method: 'getRuntimeInfo', args: [] },
                }))
                await vi.waitFor(() => expect(postMessage).toHaveBeenCalled())
                const response = postMessage.mock.calls[0][0]
                expect(response).toEqual({
                    type: 'RESPONSE',
                    reqId,
                    provenance: [],
                    result: {
                        app: { id: 'risunest', version: versionData.version },
                        apiVersion: '3.0',
                        platform: 'web',
                        saveMethod: 'local',
                        structuredLocalPluginStorage: false,
                    },
                })
                response.result.app.id = 'modified'
                response.result.app.version = 'modified'
            }
        } finally {
            postMessage.mockRestore()
            host.terminate()
        }
    })
})

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
                    `https://${domain}../`,
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
