import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { SandboxHost } from './factory'
import {
    createProductionPluginDatabaseAccess,
    linkPluginQueryAbortSignals,
} from '../pluginDatabaseAccess'
import {
    executePluginV3,
    getV3PluginInstance,
    loadV3Plugins,
    areV3PluginsIdle,
    prepareV3PluginsForReload,
    fencePluginExecutionForAuthorityReplacement,
    restartPluginsAfterAuthorityReplacement,
    canInterruptV3Plugins,
} from './v3.svelte'
import { registerOwnedPluginMCP } from '../hostToolHost'
import { requestChatDataMain } from '../../process/request/request'
import { doingChat } from '../../process/index.svelte'
import { additionalMessageButtons } from '../messageButtons.svelte'
import { chatViewEvents } from '../chatViewHost.svelte'
import { beginGenerationEndRun, noteGenerationStarted, type GenerationEndStatus } from '../../process/generationEnd'
import { language } from 'src/lang'
import { downloadFile } from 'src/ts/globalApi.svelte'
import { get } from 'svelte/store'
import { configurePluginStartup, pluginStartupGate } from '../pluginStartupGate'
import { registerSyncBindingTransport } from '../../storage/sync/bindingRegistry'
import { createStoreStub } from '../../storage/persistentDataStore.testSupport'
import type { NativeBlockRestoreRuntime } from '../../storage/nativeFileJobs'

const mocks = vi.hoisted(() => ({
    native: false,
    realRestore: false,
    invoke: vi.fn(),
    database: null as any,
    selectedId: 0,
    confirm: vi.fn(async () => true),
    permissionValues: new Map<string, unknown>(),
    providers: new Map<string, Function>(),
    cancelReload: vi.fn(),
    patchConversation: vi.fn(async (): Promise<unknown> => ({ status: 'applied', revision: 4 })),
    listHostTools: vi.fn(async (_owner: string): Promise<unknown> => ({ scope: 'scope', tools: [] })),
    callHostTool: vi.fn(async (_owner: string, _request: unknown, _signal?: AbortSignal): Promise<unknown> => []),
    chatViewFrames: [] as Array<() => void>,
    locateGenerationEnd: vi.fn(async () => ({ characterIndex: 1, chatIndex: 0, messageIndex: 3, messageId: 'gen-1' })),
}))

vi.mock('../plugins.svelte', async () => (await import('./v3Host.testSupport')).pluginsModule({
    pluginV2: { providers: mocks.providers, providerOptions: new Map() },
    cancelPluginReloadAfterSync: mocks.cancelReload,
    loadPluginsAfterAuthoritativeRestore: async () => {
        if (mocks.realRestore) await (await vi.importActual<typeof import('../plugins.svelte')>('../plugins.svelte')).loadPluginsAfterAuthoritativeRestore()
    },
}))
vi.mock('@tauri-apps/api/core', async original => ({ ...await original<object>(), invoke: mocks.invoke }))
vi.mock('src/ts/storage/database.svelte', async () => (await import('./v3Host.testSupport')).databaseModule({
    read: () => mocks.database,
    write: (database) => { mocks.database = database },
    selectedIndex: () => mocks.selectedId,
}))
vi.mock('../pluginSafeClass', async () => (await import('./v3Host.testSupport')).pluginSafeClassModule({ tagWhitelist: ['button', 'span', 'div'] }))
vi.mock('src/ts/stores.svelte', async () => (await import('./v3Host.testSupport')).storesModule({
    read: () => mocks.database,
    write: (database) => { mocks.database = database },
    selectedIndex: () => mocks.selectedId,
}))
vi.mock('src/ts/alert', async () => (await import('./v3Host.testSupport')).alertModule({ alertConfirm: mocks.confirm }))
vi.mock('src/ts/util', async () => (await import('./v3Host.testSupport')).utilModule())
vi.mock('src/ts/globalApi.svelte', async () => (await import('./v3Host.testSupport')).globalApiModule())
vi.mock('src/ts/gui/colorscheme', async () => (await import('./v3Host.testSupport')).colorschemeModule())
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule(() => ({ runtime: mocks.native ? 'tauri' : 'web', os: 'windows' })))
vi.mock('src/ts/process/mcp/pluginmcp', async () => (await import('./v3Host.testSupport')).pluginMcpModule())
vi.mock('src/ts/process/files/inlays', async () => (await import('./v3Host.testSupport')).inlaysModule())
vi.mock('src/ts/translator/translator', async () => (await import('./v3Host.testSupport')).translatorModule())
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('localforage', async () => (await import('../localforage.testSupport')).localforageModule(mocks.permissionValues))
vi.mock('src/ts/process/index.svelte', async () => (await import('./v3Host.testSupport')).processIndexModule())
vi.mock('src/ts/model/modellist', async () => (await import('./v3Host.testSupport')).modellistModule())
vi.mock('src/ts/process/request/request', async () => (await import('./v3Host.testSupport')).requestModule())
vi.mock('src/ts/process/modules', async () => (await import('./v3Host.testSupport')).modulesModule())
vi.mock('src/ts/process/ttsHooks', async () => (await import('./v3Host.testSupport')).ttsHooksModule())
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => (await import('./v3Host.testSupport')).persistentRuntimeModule())
vi.mock('../pluginDatabaseAccess', async () => (await import('./v3Host.testSupport')).pluginDatabaseAccessModule())
vi.mock('../pluginChatOutputListeners', () => ({
    registerChatOutputListener: vi.fn(), removeChatOutputListener: vi.fn(),
}))
vi.mock('src/ts/conversationMutations', () => ({ appendCurrentConversationMessage: vi.fn() }))
vi.mock('../hostToolHost', () => ({
    hostToolBridge: {
        forPlugin: (owner: string) => ({
            listTools: () => mocks.listHostTools(owner),
            callTool: (request: unknown, signal?: AbortSignal) => mocks.callHostTool(owner, request, signal),
        }),
    },
    registerOwnedPluginMCP: vi.fn(),
}))
vi.mock('../chatViewHost.svelte', async () => {
    const { createChatViewEvents } = await import('../chatViewEvents')
    let nextId = 0
    return {
        chatViewEvents: createChatViewEvents({
            readConversation: () => ({ characterId: 'char', conversationId: 'conv', characterIndex: 1, chatIndex: 0 }),
            watchConversation: () => () => undefined,
            requestFrame: (callback) => { mocks.chatViewFrames.push(callback) },
            createId: () => `chat-view-${++nextId}`,
        }),
    }
})
vi.mock('../generationEndHost', async () => {
    const { createGenerationEndEvents } = await import('../generationEndEvents')
    const { subscribeGenerationEnd } = await import('../../process/generationEnd')
    let nextId = 0
    return {
        generationEndEvents: createGenerationEndEvents({
            subscribe: subscribeGenerationEnd,
            locate: mocks.locateGenerationEnd,
            createId: () => `generation-end-${++nextId}`,
        }),
    }
})
vi.mock('../conversationPatchAccess', () => ({
    createConversationPatchAccess: () => ({ patchConversation: mocks.patchConversation }),
}))

const pluginName = 'dom-rpc-fixture'
const happyDOMWindow = window as unknown as Window & {
    happyDOM: { settings: { enableJavaScriptEvaluation: boolean } }
}
let guestEvaluation = Promise.resolve<unknown>(undefined)
let guestEvaluations: Promise<unknown>[] = []

const fixtureScript = `
globalThis.rpcState = {
    targetClicks: 0,
    documentClicks: 0,
    mutationType: '',
    mutationTargetMatches: false,
    addedChildId: '',
};
globalThis.rpcReady = (async () => {
    await risuai.requestPluginPermission('provider');
    const root = await risuai.getRootDocument();
    if (!root) throw new Error('mainDom permission was unexpectedly denied');
    const target = await root.querySelector('#plugin-target');
    if (!target) throw new Error('fixture target is missing');

    await target.addEventListener('click', (event) => {
        if (event.type === 'click') globalThis.rpcState.targetClicks += 1;
    });
    await root.addEventListener('click', () => {
        globalThis.rpcState.documentClicks += 1;
    });

    const observer = await risuai.createMutationObserver(async (records) => {
        const record = await records.at(0);
        if (!record) throw new Error('mutation record is missing');
        const mutationTarget = await record.getTarget();
        const addedNodes = await record.getAddedNodes();
        const firstAdded = await addedNodes.at(0);
        globalThis.rpcState.mutationType = await record.getType();
        globalThis.rpcState.mutationTargetMatches = await mutationTarget.matches('#plugin-target');
        globalThis.rpcState.addedChildId = firstAdded
            ? await firstAdded.getAttribute('x-fixture-child') || ''
            : '';
    });
    await observer.observe(target, { childList: true });
    globalThis.fixtureTarget = target;
})();
`

function installFixtureDatabase(name = pluginName, script = fixtureScript): void {
    mocks.database = {
        aiModel: 'fixture-model',
        characters: [],
        plugins: [{ name, script }],
    }
}

async function guest(name: string, code: string): Promise<unknown> {
    const instance = getV3PluginInstance(name)
    if (!instance) throw new Error(`missing fixture plugin ${name}`)
    expect(instance.host).toBeInstanceOf(SandboxHost)
    return instance.host.executeInIframe(code)
}

async function waitFor<T>(read: () => Promise<T>, matches: (value: T) => boolean): Promise<T> {
    for (let attempt = 0; attempt < 50; attempt += 1) {
        const value = await read()
        if (matches(value)) return value
        await new Promise<void>((resolve) => setTimeout(resolve, 0))
    }
    throw new Error('fixture condition did not settle')
}

async function readState(name: string) {
    return JSON.parse(String(await guest(
        name,
        'await globalThis.rpcReady; return JSON.stringify(globalThis.rpcState)',
    ))) as {
        targetClicks: number
        documentClicks: number
        mutationType: string
        mutationTargetMatches: boolean
        addedChildId: string
    }
}

async function startFixture(name = pluginName, script = fixtureScript): Promise<void> {
    installFixtureDatabase(name, script)
    await executePluginV3({ name, script } as any)
    await guestEvaluation
    await guest(name, 'await globalThis.rpcReady; return true')
}

async function gatedRestoreFixture() {
    mocks.native = true
    mocks.realRestore = true
    mocks.invoke.mockImplementation(async command => {
        if (command === 'pds_lww_binding_state') return { target: {kind:'server',connectionId:'gated-fixture'}, targetAuthority:'1', selectionEpoch:'1', libraryId:'synthetic', progress:null }
        if (command === 'pds_close_plugin_claim_eligibility') return
        throw new Error(`Unexpected native command ${command}`)
    })
    const receives: Array<{signal:AbortSignal; resolve():void; reject(error:unknown):void}> = []
    const unexpected = async (): Promise<never> => { throw new Error('Unexpected binding operation') }
    const dispose = registerSyncBindingTransport({kind:'server',connectionId:'gated-fixture'}, {
        inspectTarget:unexpected, pullAvailableState:unexpected, replaceFromTarget:unexpected,
        publishInitialSharedState:unexpected, resumeBinding:unexpected, fenceOldJobs:unexpected,
        receiveAvailableChanges: context => new Promise<void>((resolve,reject) => { receives.push({signal:context.signal,resolve,reject}) }),
    })
    const plugins = await vi.importActual<typeof import('../plugins.svelte')>('../plugins.svelte')
    configurePluginStartup(() => true, () => plugins.loadPlugins())
    mocks.database.plugins = [{name:'gated-successor',version:'3.0',enabled:true,script:'globalThis.rpcReady = Promise.resolve()',realArg:{},arguments:{},customLink:[],argMeta:{}}]
    return {plugins,receives,dispose}
}

describe('Plugin v3 real iframe DOM/RPC bridge', () => {
    it('routes unchanged plugin anchors to the native file save boundary', async () => {
        const saved: { name: string; bytes: number[] }[] = []
        vi.mocked(downloadFile).mockImplementationOnce(async (name, data) => {
            saved.push({ name, bytes: [...new Uint8Array(data as Uint8Array)] })
            return true
        })
        mocks.native = true
        await startFixture('native-download', `
            const a = document.createElement('a');
            a.href = 'data:application/octet-stream;base64,AP8NCg==';
            a.download = 'plugin.risup';
            a.click();
        `)
        await vi.waitFor(() => expect(saved).toEqual([{ name: 'plugin.risup', bytes: [0, 255, 13, 10] }]))
    })

    beforeEach(() => {
        vi.clearAllMocks()
        mocks.native = false
        mocks.realRestore = false
        pluginStartupGate.invalidate()
        pluginStartupGate.state.set({phase:'released',since:0})
        configurePluginStartup(() => false, async () => {})
        doingChat.set(false)
        vi.stubGlobal('ImageBitmap', class {})
        const srcdocDescriptor = Object.getOwnPropertyDescriptor(HTMLIFrameElement.prototype, 'srcdoc')
        let frameSequence = 0
        vi.spyOn(HTMLIFrameElement.prototype, 'srcdoc', 'set').mockImplementation(function (value: string) {
            frameSequence += 1
            const frameId = `dom-rpc-frame-${frameSequence}`
            const transportShim = `
globalThis.ImageBitmap = class {};
function __postToParent(message) {
    const source = parent.document.querySelector('iframe[data-risu-test-frame="${frameId}"]').contentWindow;
    const data = __cloneForParent(message);
    parent.dispatchEvent(new parent.MessageEvent('message', { data, source }));
}
`
            const sourceValue = value
                .replace(/window\.parent\.postMessage/g, '__postToParent')
                .replace(/parent\.postMessage/g, '__postToParent')
                .replace(/(<script nonce="[^"]+">)/, `$1${transportShim}`)
            const settings = happyDOMWindow.happyDOM.settings
            settings.enableJavaScriptEvaluation = false
            srcdocDescriptor?.set?.call(this, sourceValue)
            settings.enableJavaScriptEvaluation = true

            this.setAttribute('data-risu-test-frame', frameId)
            const child = this.contentWindow!
            const childRealm = child as any
            childRealm.__cloneForParent = (message: unknown) => structuredClone(message)
            vi.spyOn(child, 'postMessage').mockImplementation((data) => {
                child.dispatchEvent(new childRealm.MessageEvent('message', {
                    data: structuredClone(data),
                    source: child.parent,
                }))
            })
            const source = sourceValue.match(/<script nonce="[^"]+">([\s\S]*)<\/script>/)?.[1]
            if (!source) throw new Error('Sandbox guest script was not found')
            guestEvaluation = childRealm.eval(source)
            guestEvaluations.push(guestEvaluation)
        })
        mocks.permissionValues.clear()
        mocks.providers.clear()
        mocks.cancelReload.mockReset()
        guestEvaluations = []
        mocks.confirm.mockResolvedValue(true)
        mocks.selectedId = 0
        happyDOMWindow.happyDOM.settings.enableJavaScriptEvaluation = true
        document.body.replaceChildren()
        document.body.insertAdjacentHTML('beforeend', [
            '<button id="plugin-target">plugin</button>',
            '<button id="other-target">other</button>',
        ].join(''))
        installFixtureDatabase()
    })

    afterEach(async () => {
        mocks.realRestore = false
        mocks.native = false
        pluginStartupGate.invalidate()
        configurePluginStartup(() => false, async () => {})
        await restartPluginsAfterAuthorityReplacement()
        await loadV3Plugins([])
        document.body.replaceChildren()
        vi.unstubAllEnvs()
        vi.restoreAllMocks()
    })

    it('returns method-specific icon errors through the real bridge without argument contents', async () => {
        await startFixture('icon-errors-fixture')
        const errors = JSON.parse(
            String(
                await guest(
                    'icon-errors-fixture',
                    `
            const errors = [];
            for (const call of [
                () => risuai.registerSetting('fixture', () => {}, '', 'secret-fixture'),
                () => risuai.registerButton({ name: 'fixture', icon: '', iconType: 'secret-fixture' }, () => {}),
                () => risuai.registerButton(null, () => {}),
            ]) {
                try { await call(); } catch (error) { errors.push(error.message); }
            }
            return JSON.stringify(errors);
        `,
                ),
            ),
        ) as string[]
        expect(errors[0]).toMatch(/^\[Plugin API: registerSetting\]/)
        expect(errors[0]).toContain(
            "registerSetting: fourth argument iconType must be 'html', 'img' or 'none'",
        )
        expect(errors[1]).toContain(
            "registerButton: options.iconType must be 'html', 'img' or 'none'",
        )
        expect(errors[2]).toContain(
            'registerButton: first argument must be an options object',
        )
        expect(errors.join()).not.toContain('secret-fixture')
    })

    it('identifies the originating plugin in uncaught guest code stacks', async () => {
        await startFixture(
            'stack-fixture',
            `
            globalThis.rpcReady = Promise.resolve();
            globalThis.throwFixture = () => { throw new TypeError('synthetic failure'); };
        `,
        )
        const stack = String(
            await guest(
                'stack-fixture',
                `
            try { globalThis.throwFixture(); } catch (error) { return error.stack; }
        `,
            ),
        )
        expect(stack).toContain('risu-plugin-v3/stack-fixture.js')
        expect(stack).toContain('TypeError: synthetic failure')
    })

    it('does not reuse a provider permission decision for mainDom', async () => {
        await startFixture()

        expect(mocks.confirm).toHaveBeenCalledTimes(2)
    })

    it('does not invoke a provider callback when provider permission is denied', async () => {
        const name = 'denied-provider-fixture'
        mocks.confirm.mockResolvedValue(false)
        await startFixture(name, `
            globalThis.providerCalls = 0;
            globalThis.rpcReady = risuai.addProvider('denied-provider', async () => {
                globalThis.providerCalls += 1;
                return { success: true, content: 'unexpected' };
            });
        `)

        const provider = mocks.providers.get('denied-provider')
        expect(provider).toBeTypeOf('function')
        const result = await provider!({
            prompt_chat: [{ role: 'user', content: 'sensitive sentinel' }],
            mode: 'chat',
        })

        expect(result).toEqual({ success: false, content: language.pluginProviderPermissionDenied })
        expect(await guest(name, 'return globalThis.providerCalls')).toBe(0)
    })

    it('binds a SafeElement listener to its element and keeps SafeDocument at document scope', async () => {
        await startFixture()
        document.querySelector('#other-target')?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
        document.querySelector('#plugin-target')?.dispatchEvent(new MouseEvent('click', { bubbles: true }))
        document.dispatchEvent(new Event('click'))

        const state = await waitFor(
            () => readState(pluginName),
            (value) => value.targetClicks === 1 && value.documentClicks === 3,
        )
        expect(state).toMatchObject({ targetClicks: 1, documentClicks: 3 })
    })

    it('removes exactly the listener its ID names even when the removal type and options differ', async () => {
        const name = 'listener-removal'
        await startFixture(name, `
            globalThis.counts = { captured: 0, kept: 0 };
            globalThis.rpcReady = (async () => {
                const root = await risuai.getRootDocument();
                globalThis.target = await root.querySelector('#plugin-target');
                globalThis.capturedId = await globalThis.target.addEventListener('click', () => {
                    globalThis.counts.captured += 1;
                }, { capture: true });
                await globalThis.target.addEventListener('click', () => {
                    globalThis.counts.kept += 1;
                });
            })();
        `)
        const counts = () => guest(name, 'return JSON.stringify(globalThis.counts)').then(value => JSON.parse(String(value)))
        const click = () => document.querySelector('#plugin-target')!.dispatchEvent(new MouseEvent('click', { bubbles: true }))

        click()
        await waitFor(counts, (value) => value.kept === 1)
        await guest(name, "await globalThis.target.removeEventListener('dblclick', globalThis.capturedId, false); return true")
        click()

        expect(await waitFor(counts, (value) => value.kept === 2)).toEqual({ captured: 1, kept: 2 })
    })

    it('delivers MutationObserver records as callable remote proxies', async () => {
        await startFixture()
        const child = document.createElement('span')
        child.setAttribute('x-fixture-child', 'added')
        document.querySelector('#plugin-target')?.append(child)

        const state = await waitFor(
            () => readState(pluginName),
            (value) => value.addedChildId === 'added',
        )
        expect(state).toMatchObject({
            mutationType: 'childList',
            mutationTargetMatches: true,
            addedChildId: 'added',
        })
    })

    it('terminates both existing plugin iframes before a reload', async () => {
        await loadV3Plugins([
            { name: 'first-fixture', script: '' },
            { name: 'second-fixture', script: '' },
        ] as any)
        await Promise.all(guestEvaluations)
        expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(2)

        await loadV3Plugins([])

        expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(0)
    })

    it('allows onUnload to reacquire the main document and remove its own DOM', async () => {
        await startFixture('cleanup-dom', `
            globalThis.rpcReady = (async () => {
                await risuai.getRootDocument();
                await risuai.onUnload(async () => {
                    const root = await risuai.getRootDocument();
                    const target = await root.querySelector('#plugin-target');
                    await target.remove();
                });
            })();
        `)
        await loadV3Plugins([])
        expect(document.querySelector('#plugin-target')).toBeNull()
        expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(0)
    })

    it('stays busy during a guest provider callback and during chat generation', async () => {
        const name = 'busy-provider'
        await startFixture(name, `
            globalThis.rpcReady = risuai.addProvider('busy', () => new Promise(resolve => {
                globalThis.finishProvider = () => resolve({success:true, content:'done'});
            }));
        `)
        expect(areV3PluginsIdle()).toBe(true)
        const result = mocks.providers.get('busy')!({})
        await vi.waitFor(async () => expect(await guest(name, 'return typeof globalThis.finishProvider')).toBe('function'))
        expect(areV3PluginsIdle()).toBe(false)
        await guest(name, 'globalThis.finishProvider(); return true')
        expect(await result).toEqual({success:true, content:'done'})
        expect(areV3PluginsIdle()).toBe(true)
        doingChat.set(true)
        expect(areV3PluginsIdle()).toBe(false)
        doingChat.set(false)
        expect(areV3PluginsIdle()).toBe(true)
    })

    it('lets a reload interrupt a plugin script that never settles, but never a chat generation', async () => {
        await startFixture('never-settles', `
            globalThis.rpcReady = Promise.resolve();
            await new Promise(() => {});
        `)
        expect(areV3PluginsIdle()).toBe(false)
        expect(prepareV3PluginsForReload()).toBe(false)
        doingChat.set(true)
        expect(prepareV3PluginsForReload(true)).toBe(false)
        doingChat.set(false)
        expect(prepareV3PluginsForReload(true)).toBe(true)
        await loadV3Plugins([])
        expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(0)
    })

    it('aborts a running model request when its sandbox unloads', async () => {
        let signal: AbortSignal | undefined
        vi.mocked(requestChatDataMain).mockImplementationOnce((_arg, _mode, abortSignal) => new Promise(resolve => {
            signal = abortSignal
            signal!.addEventListener('abort', () => resolve({type:'fail', result:'aborted'}), {once:true})
        }))
        await startFixture('model-lifetime', `
            globalThis.rpcReady = Promise.resolve();
            void risuai.runLLMModel({mode:'model', messages:[]});
        `)
        await vi.waitFor(() => expect(signal).toBeDefined())
        expect(signal?.aborted).toBe(false)
        await loadV3Plugins([])
        expect(signal?.aborted).toBe(true)
    })

    it('disposes an MCP registration made through the guest API', async () => {
        const dispose = vi.fn()
        vi.mocked(registerOwnedPluginMCP).mockResolvedValueOnce(dispose)
        await startFixture('mcp-lifetime', `
            globalThis.rpcReady = risuai.registerMCP({identifier:'plugin:fixture', name:'Fixture', version:'1', description:''}, () => [], () => []);
        `)
        await loadV3Plugins([])
        expect(dispose).toHaveBeenCalledOnce()
    })

    it('permits plugin reload after restore preparation fails', async () => {
        mocks.cancelReload.mockImplementationOnce(() => { throw new Error('Synthetic preparation failure') })
        await expect(fencePluginExecutionForAuthorityReplacement()).rejects.toThrow('Synthetic preparation failure')
        expect(canInterruptV3Plugins()).toBe(true)
        await startFixture('after-failed-restore')
        expect(getV3PluginInstance('after-failed-restore')).toBeDefined()
    })

    it.each(['pre-aborted','preparation-failure'] as const)('restarts through the actual startup gate after %s and retries failed readiness', async failure => {
        const h = await gatedRestoreFixture()
        try {
            await h.plugins.loadPlugins()
            expect(h.receives).toHaveLength(1)
            const controller = new AbortController()
            if (failure === 'pre-aborted') controller.abort()
            else mocks.cancelReload.mockImplementationOnce(() => { throw new Error('Synthetic preparation failure') })
            await expect(fencePluginExecutionForAuthorityReplacement(controller.signal)).rejects.toThrow()
            expect(h.receives[0].signal.aborted).toBe(true)
            await restartPluginsAfterAuthorityReplacement()
            expect(h.receives).toHaveLength(2)
            h.receives[1].reject(new Error('Synthetic readiness failure'))
            await vi.waitFor(() => expect(get(pluginStartupGate.state).phase).toBe('failed'))
            expect(getV3PluginInstance('gated-successor')).toBeUndefined()
            pluginStartupGate.retry()
            await vi.waitFor(() => expect(h.receives).toHaveLength(3))
            h.receives[2].resolve()
            await vi.waitFor(() => expect(getV3PluginInstance('gated-successor')).toBeDefined())
            const instance = getV3PluginInstance('gated-successor')
            h.receives[0].resolve()
            await Promise.resolve(); await Promise.resolve()
            expect(getV3PluginInstance('gated-successor')).toBe(instance)
        } finally { h.dispose() }
    })

    it('recovers a native preparation deadline through the real fence, restart and startup gate', async () => {
        await startFixture('deadline-restore', `globalThis.rpcReady = risuai.onUnload(() => new Promise(resolve => { globalThis.finishCleanup = resolve; }));`)
        const child = getV3PluginInstance('deadline-restore')!.host
        const h = await gatedRestoreFixture()
        mocks.invoke.mockImplementation(async command => {
            if (command === 'pds_lww_binding_state') return {target:{kind:'none'},targetAuthority:'1',selectionEpoch:'1',libraryId:null,progress:null}
            if (command === 'pds_close_plugin_claim_eligibility') return
            throw new Error(`Unexpected native command ${command}`)
        })
        const unexpected = (): never => { throw new Error('Replacement must not be submitted') }
        const runtime = {
            capturePersistentMutationToken:async()=>({revision:1,mutationGeneration:0}),
            acquireDestructiveReplacementFence:unexpected, withPausedPersistentWrites:unexpected,
            beginActivatedLibraryGuard:unexpected, refreshActivatedLibraryUnderPause:unexpected,
            markCommittedWorkingSetRefreshRequired:unexpected, setActivatedLibraryRecoveryLifecycle:unexpected,
            getStorageAuthorityEpoch:()=>0, store:createStoreStub({}),
        } satisfies NativeBlockRestoreRuntime
        const deadlines = new Set<() => void>()
        let cancelled = false
        const commands: string[] = []
        const {runNativeBlockRisuSaveRestore} = await import('../../storage/nativeFileJobs')
        try {
            const restore = runNativeBlockRisuSaveRestore(runtime,{type:'desktopPath',path:'synthetic.risudat'},{firstRun:true},{
                isTauri:()=>true, wait:async()=>{},
                restoreClock:{schedule(_milliseconds,callback){deadlines.add(callback);return()=>{deadlines.delete(callback)}}},
                invoke:async command=>{
                    commands.push(command)
                    if(command==='native_file_job_start') return {jobId:'deadline-job'}
                    if(command==='native_file_job_status') return {kind:'restore-block-risu-save',state:cancelled?'cancelled':'waitingForInput',phase:cancelled?'cancelled':'awaiting-activation'}
                    if(command==='native_file_job_cancel'){cancelled=true;return {outcome:'requested'}}
                    if(command==='native_file_job_forget')return true
                    throw new Error(`Unexpected native command ${command}`)
                },
            })
            const outcome = expect(restore).rejects.toMatchObject({code:'restore-control-timeout'})
            await vi.waitFor(async()=>expect(await child.executeInIframe('return typeof globalThis.finishCleanup')).toBe('function'))
            for(const expire of [...deadlines]) expire()
            await vi.waitFor(()=>expect(cancelled).toBe(true))
            await child.executeInIframe('globalThis.finishCleanup(); return true')
            await outcome
            await vi.waitFor(()=>expect(getV3PluginInstance('gated-successor')).toBeDefined())
            expect(commands).not.toContain('native_file_job_finalize')
            expect(get(pluginStartupGate.state).phase).toBe('released')
        } finally { h.dispose() }
    })

    it('cancels a stalled restore fence without letting late teardown erase the next plugin load', async () => {
        await startFixture('cancelled-restore', `globalThis.rpcReady = risuai.onUnload(() => new Promise(resolve => { globalThis.finishCleanup = resolve; }));`)
        const child = getV3PluginInstance('cancelled-restore')!.host
        const controller = new AbortController()
        const fencing = fencePluginExecutionForAuthorityReplacement(controller.signal)
        const cancelled = expect(fencing).rejects.toMatchObject({name:'AbortError'})
        await vi.waitFor(async () => expect(await child.executeInIframe('return typeof globalThis.finishCleanup')).toBe('function'))
        const h = await gatedRestoreFixture()
        controller.abort()
        expect(canInterruptV3Plugins()).toBe(true)
        try {
            await restartPluginsAfterAuthorityReplacement()
            expect(h.receives).toHaveLength(1)
            h.receives[0].resolve()
            await child.executeInIframe('globalThis.finishCleanup(); return true')
            await cancelled
            await vi.waitFor(() => expect(getV3PluginInstance('gated-successor')).toBeDefined())
            expect(getV3PluginInstance('cancelled-restore')).toBeUndefined()
        } finally { h.dispose() }
    })

    it('serializes overlapping reloads and invokes guest cleanup once', async () => {
        await startFixture('overlapping-reload', `
            globalThis.cleanupCalls = 0;
            globalThis.rpcReady = risuai.onUnload(() => {
                globalThis.cleanupCalls++;
                return new Promise(resolve => { globalThis.finishCleanup = resolve; });
            });
        `)
        const child = getV3PluginInstance('overlapping-reload')!.host
        const first = loadV3Plugins([])
        const second = loadV3Plugins([])
        await vi.waitFor(async () => expect(await child.executeInIframe('return typeof globalThis.finishCleanup')).toBe('function'))
        expect(await child.executeInIframe('return globalThis.cleanupCalls')).toBe(1)
        await child.executeInIframe('globalThis.finishCleanup(); return true')
        await Promise.all([first, second])
        expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(0)
    })

    it('finishes host teardown after an unload callback rejects', async () => {
        await startFixture('cleanup-error', `
            globalThis.rpcReady = risuai.onUnload(() => { throw new Error('synthetic cleanup failure'); });
        `)
        await loadV3Plugins([])
        expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(0)
    })

    it('limits an unsettled unload callback to one second', async () => {
        await startFixture('cleanup-timeout', `
            globalThis.rpcReady = risuai.onUnload(() => new Promise(() => {}));
        `)
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        try {
            const unloading = loadV3Plugins([])
            await vi.advanceTimersByTimeAsync(999)
            expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(1)
            await vi.advanceTimersByTimeAsync(1)
            await unloading
            expect(document.querySelectorAll('iframe[data-risu-plugin-frame]')).toHaveLength(0)
        } finally { vi.useRealTimers() }
    })

    describe('plugin channel IPC', () => {
        const ipcScript = (channel: string) => `
            globalThis.received = [];
            globalThis.rpcReady = risuai.addPluginChannelListener(${JSON.stringify(channel)}, (message, meta) => {
                globalThis.received.push({ message, sender: meta.sender, channel: meta.channel });
            });
        `

        async function startIpcPlugins(plugins: { name: string; allowedIPC: string[]; channel?: string }[]): Promise<void> {
            const records = plugins.map((plugin) => ({
                name: plugin.name,
                script: ipcScript(plugin.channel ?? 'inbox'),
                allowedIPC: plugin.allowedIPC,
            }))
            mocks.database = { aiModel: 'fixture-model', characters: [], plugins: records }
            await loadV3Plugins(records as any)
            await Promise.all(guestEvaluations)
            for (const plugin of plugins) await guest(plugin.name, 'await globalThis.rpcReady; return true')
        }

        async function post(sender: string, receiver: string, channel: string, message: unknown): Promise<unknown> {
            return guest(sender, `
                const result = await risuai.postPluginChannelMessage(${JSON.stringify(receiver)}, ${JSON.stringify(channel)}, ${JSON.stringify(message)});
                return result === undefined ? 'resolved' : 'unexpected';
            `)
        }

        async function received(name: string) {
            return JSON.parse(String(await guest(name, 'return JSON.stringify(globalThis.received)'))) as {
                message: unknown
                sender: string
                channel: string
            }[]
        }

        async function settle(): Promise<void> {
            for (let attempt = 0; attempt < 20; attempt += 1) {
                await new Promise<void>((resolve) => setTimeout(resolve, 0))
            }
        }

        it('delivers both ways between a wildcard plugin and a client that names it, with the host-attested sender', async () => {
            vi.spyOn(console, 'warn').mockImplementation(() => undefined)
            await startIpcPlugins([
                { name: 'hub', allowedIPC: ['hub', '*'] },
                { name: 'client', allowedIPC: ['client', 'hub'] },
            ])

            expect(await post('client', 'hub', 'inbox', { register: true })).toBe('resolved')
            expect(await post('hub', 'client', 'inbox', { reply: true })).toBe('resolved')

            expect(await waitFor(() => received('hub'), (value) => value.length === 1)).toEqual([
                { message: { register: true }, sender: 'client', channel: 'inbox' },
            ])
            expect(await waitFor(() => received('client'), (value) => value.length === 1)).toEqual([
                { message: { reply: true }, sender: 'hub', channel: 'inbox' },
            ])
        })

        it('keeps the two-sided rule when only one side lists the other or a wildcard', async () => {
            const warn = vi.spyOn(console, 'warn').mockImplementation(() => undefined)
            await startIpcPlugins([
                { name: 'hub', allowedIPC: ['hub', '*'] },
                { name: 'stranger', allowedIPC: ['stranger'] },
                { name: 'one-sided', allowedIPC: ['one-sided', 'plain'] },
                { name: 'plain', allowedIPC: ['plain'] },
            ])

            expect(await post('stranger', 'hub', 'inbox', 1)).toBe('resolved')
            expect(await post('hub', 'stranger', 'inbox', 2)).toBe('resolved')
            expect(await post('one-sided', 'plain', 'inbox', 3)).toBe('resolved')
            expect(await post('plain', 'one-sided', 'inbox', 4)).toBe('resolved')
            expect(await post('hub', 'missing-plugin', 'inbox', 5)).toBe('resolved')
            await settle()

            for (const name of ['hub', 'stranger', 'one-sided', 'plain']) {
                expect(await received(name)).toEqual([])
            }
            expect(warn.mock.calls.filter(([text]) => String(text).includes('Attempted to send message'))).toHaveLength(5)
        })

        it('delivers between plugins that name each other without a wildcard', async () => {
            await startIpcPlugins([
                { name: 'left', allowedIPC: ['left', 'right'] },
                { name: 'right', allowedIPC: ['right', 'left'] },
            ])

            expect(await post('left', 'right', 'inbox', 'hello')).toBe('resolved')

            expect(await waitFor(() => received('right'), (value) => value.length === 1)).toEqual([
                { message: 'hello', sender: 'left', channel: 'inbox' },
            ])
        })

        it('keeps listeners apart when plugin and channel names concatenate to the same string', async () => {
            await startIpcPlugins([
                { name: 'sender', allowedIPC: ['*'] },
                { name: 'a', allowedIPC: ['*'], channel: 'bc' },
                { name: 'ab', allowedIPC: ['*'], channel: 'c' },
            ])

            expect(await post('sender', 'a', 'bc', 'for a')).toBe('resolved')
            expect(await post('sender', 'ab', 'c', 'for ab')).toBe('resolved')

            expect(await waitFor(() => received('a'), (value) => value.length === 1)).toEqual([
                { message: 'for a', sender: 'sender', channel: 'bc' },
            ])
            expect(await waitFor(() => received('ab'), (value) => value.length === 1)).toEqual([
                { message: 'for ab', sender: 'sender', channel: 'c' },
            ])
        })
    })

    it('reaches the private conversation context read from the guest with the db decision', async () => {
        const name = 'context-reader'
        const readConversationContext = vi.fn(async () => ({ revision: 7, characterId: 'char' }))
        vi.mocked(createProductionPluginDatabaseAccess).mockReturnValueOnce({ readConversationContext } as never)
        vi.mocked(linkPluginQueryAbortSignals).mockImplementation((...signals) => {
            const controller = new AbortController()
            for (const signal of signals) signal?.addEventListener('abort', () => controller.abort(signal.reason))
            return { signal: controller.signal, dispose() {} }
        })
        mocks.confirm.mockResolvedValueOnce(false)
        await startFixture(name, 'globalThis.rpcReady = Promise.resolve()')

        const result = await guest(name, `
            const context = await risuai.risunestReadConversationContext({
                characterId: 'char',
                conversationId: 'conv',
                include: { persona: true },
                messages: { limit: 2 },
            });
            return JSON.stringify(context);
        `)

        expect(JSON.parse(String(result))).toEqual({ revision: 7, characterId: 'char' })
        expect(mocks.confirm).toHaveBeenCalledTimes(1)
        expect(readConversationContext).toHaveBeenCalledWith(
            expect.objectContaining({
                target: { characterId: 'char', conversationId: 'conv' },
                messages: { window: { limit: 2 }, extraFields: [] },
            }),
            { allowPrivate: false, signal: expect.any(AbortSignal) },
        )
    })

    it('reaches the private conversation patch from the guest with removals kept', async () => {
        const name = 'conversation-patcher'
        vi.mocked(linkPluginQueryAbortSignals).mockImplementation(() => ({ signal: new AbortController().signal, dispose() {} }))
        await startFixture(name, 'globalThis.rpcReady = Promise.resolve()')

        const result = await guest(name, `
            const outcome = await risuai.risunestPatchConversation({
                characterId: 'char',
                conversationId: 'conv',
                mutationId: 'guest-patch',
                messages: [{ index: 0, messageId: 'm0', expected: { __old: undefined }, set: { __tr: 'text', __gone: undefined } }],
            });
            return JSON.stringify(outcome);
        `)

        expect(JSON.parse(String(result))).toEqual({ status: 'applied', revision: 4 })
        const [request] = mocks.patchConversation.mock.calls[0] as unknown as [{ messages: { expected: object; set: object }[] }]
        expect(request).toMatchObject({ characterId: 'char', conversationId: 'conv', mutationId: 'guest-patch' })
        expect(Object.hasOwn(request.messages[0].set, '__gone')).toBe(true)
        expect(Object.hasOwn(request.messages[0].expected, '__old')).toBe(true)
    })

    it('reaches the private host tools from the guest under their own permission', async () => {
        vi.mocked(linkPluginQueryAbortSignals).mockImplementation(() => ({ signal: new AbortController().signal, dispose() {} }))
        const listed = { scope: 'scope-a', tools: [{ source: 'internal:dice', sourceName: 'Dice', name: 'rollDice', inputSchema: { type: 'object' } }] }
        mocks.listHostTools.mockResolvedValueOnce(listed)
        mocks.callHostTool.mockResolvedValueOnce([{ type: 'text', text: 'Rolled 1d6: 4' }])
        await startFixture('tool-caller', 'globalThis.rpcReady = Promise.resolve()')

        const result = JSON.parse(String(await guest('tool-caller', `
            const list = await risuai.risunestListHostTools();
            const content = await risuai.risunestCallHostTool({ scope: list.scope, source: 'internal:dice', name: 'rollDice', arguments: { notation: '1d6' } });
            return JSON.stringify({ list, content });
        `)))
        expect(result).toEqual({ list: listed, content: [{ type: 'text', text: 'Rolled 1d6: 4' }] })
        expect(mocks.confirm).toHaveBeenCalledExactlyOnceWith(language.hostToolPermissionConsent.replace('{}', 'tool-caller'))
        expect(mocks.listHostTools).toHaveBeenCalledWith('tool-caller')
        expect(mocks.callHostTool).toHaveBeenCalledWith('tool-caller',
            { scope: 'scope-a', source: 'internal:dice', name: 'rollDice', arguments: { notation: '1d6' } }, expect.any(AbortSignal))

        mocks.confirm.mockResolvedValue(false)
        await startFixture('denied-tool-caller', 'globalThis.rpcReady = Promise.resolve()')
        const errors = JSON.parse(String(await guest('denied-tool-caller', `
            const errors = [];
            for (const call of [() => risuai.risunestListHostTools(), () => risuai.risunestCallHostTool({ scope: 's', source: 'internal:dice', name: 'rollDice' })]) {
                try { await call(); } catch (error) { errors.push(error.message); }
            }
            return JSON.stringify(errors);
        `))) as string[]
        expect(errors).toHaveLength(2)
        for (const error of errors) expect(error).toContain('hostTools permission')
        expect(mocks.listHostTools).toHaveBeenCalledTimes(1)
        expect(mocks.callHostTool).toHaveBeenCalledTimes(1)
    })

    it('tells a private method this build lacks, a declined write and an applied write apart in the guest', async () => {
        const { commitPersistentUnitIntent } = await import('src/ts/storage/persistentDataRuntime.svelte')
        const write = `
            const outcomes = [];
            try { await risuai.risunestMethodFromLaterBuild(); } catch (error) { outcomes.push(error.message); }
            outcomes.push(await risuai.risunestWriteSettings({ askRemoval: true }));
            return JSON.stringify(outcomes);
        `
        mocks.confirm.mockResolvedValueOnce(false)
        await startFixture('declined-settings-writer', 'globalThis.rpcReady = Promise.resolve()')
        const declined = JSON.parse(String(await guest('declined-settings-writer', write)))
        await startFixture('settings-writer', 'globalThis.rpcReady = Promise.resolve()')
        const applied = JSON.parse(String(await guest('settings-writer', write)))

        expect(declined[0]).toContain('API method risunestMethodFromLaterBuild not found')
        expect(declined[1]).toEqual({ status: 'denied' })
        expect(applied[1]).toEqual({ status: 'applied' })
        expect(mocks.confirm.mock.calls).toEqual([
            [language.dbWriteConsent.replace('{}', 'declined-settings-writer')],
            [language.dbWriteConsent.replace('{}', 'settings-writer')],
        ])
        expect(commitPersistentUnitIntent).toHaveBeenCalledExactlyOnceWith('plugin-settings-write', [
            { key: JSON.stringify(['root', 'askRemoval']), type: 'set', value: true },
        ])
    })

    it('registers message buttons with roles and hands the clicked message to the guest', async () => {
        const name = 'message-buttons'
        await startFixture(name, `
            globalThis.targets = [];
            globalThis.rpcReady = (async () => {
                await risuai.registerButton({ name: 'Translate', icon: '<b>T</b>', iconType: 'html', location: 'message', id: 'translate', roles: ['char', 'char'] },
                    (target) => { globalThis.targets.push(target); });
                await risuai.registerButton({ name: 'Both', icon: '', iconType: 'none', location: 'message', id: 'both' }, () => {});
            })();
        `)
        const buttons = () => additionalMessageButtons.map(({ id, name, roles }) => ({ id, name, roles }))
        expect(buttons()).toEqual([{ id: 'translate', name: 'Translate', roles: ['char'] }, { id: 'both', name: 'Both', roles: undefined }])

        const target = { characterIndex: 1, chatIndex: 0, messageIndex: 4123, messageId: 'm-4123', role: 'char', characterId: 'char', conversationId: 'conv' }
        await additionalMessageButtons[0].callback(target)
        expect(JSON.parse(String(await guest(name, 'return JSON.stringify(globalThis.targets)')))).toEqual([target])

        await guest(name, `await risuai.registerButton({ name: 'Translated', icon: '', iconType: 'none', id: 'translate', roles: ['user'] }, () => {}); return true`)
        expect(buttons()).toEqual([{ id: 'translate', name: 'Translated', roles: ['user'] }, { id: 'both', name: 'Both', roles: undefined }])

        const errors = JSON.parse(String(await guest(name, `
            const errors = [];
            for (const roles of [[], ['system'], 'char']) {
                try { await risuai.registerButton({ name: 'Bad', icon: '', iconType: 'none', location: 'message', roles }, () => {}); }
                catch (error) { errors.push(error.message); }
            }
            return JSON.stringify(errors);
        `))) as string[]
        expect(errors).toHaveLength(3)
        for (const error of errors) expect(error).toContain('options.roles')
        expect(additionalMessageButtons).toHaveLength(2)

        await guest(name, `await risuai.unregisterUIPart('both'); return true`)
        expect(buttons().map(({ id }) => id)).toEqual(['translate'])
        await loadV3Plugins([])
        expect(additionalMessageButtons).toHaveLength(0)
    })

    it('delivers chat view events to the guest until unregisterUIPart or unload', async () => {
        const name = 'chat-view'
        await startFixture(name, `
            globalThis.viewEvents = [];
            globalThis.rpcReady = (async () => {
                globalThis.viewId = (await risuai.risunestOnChatView((event) => { globalThis.viewEvents.push(event); })).id;
            })();
        `)
        const reporter = chatViewEvents.createReporter()
        const report = (index: number) => ({
            characterId: 'char', conversationId: 'conv', index, message: { role: 'char', chatId: `m-${index}` }, streaming: false,
        })
        const runFrames = () => { for (const frame of mocks.chatViewFrames.splice(0)) frame() }
        const received = async () => JSON.parse(String(await guest(name, 'return JSON.stringify(globalThis.viewEvents)')))
        try {
            reporter.rendered('row-4', report(4))
            runFrames()
            await vi.waitFor(async () => expect(await received()).toEqual([
                { type: 'conversation', characterId: 'char', conversationId: 'conv', characterIndex: 1, chatIndex: 0 },
                { type: 'rows', characterId: 'char', conversationId: 'conv', mounted: [{ index: 4, messageId: 'm-4', role: 'char' }], unmounted: [], rerendered: [] },
            ]))

            await guest(name, 'await risuai.unregisterUIPart(globalThis.viewId); return true')
            reporter.rendered('row-5', report(5))
            expect(mocks.chatViewFrames).toHaveLength(0)
            expect(await received()).toHaveLength(2)

            await guest(name, 'await risuai.risunestOnChatView(() => {}); return true')
            expect(mocks.chatViewFrames).toHaveLength(1)
            runFrames()
            await loadV3Plugins([])
            reporter.rendered('row-6', report(6))
            expect(mocks.chatViewFrames).toHaveLength(0)
        } finally {
            reporter.dispose()
        }
    })

    it('delivers generation end events to the guest until unregisterUIPart or unload', async () => {
        const name = 'generation-end'
        await startFixture(name, `
            globalThis.endEvents = [];
            globalThis.rpcReady = (async () => {
                globalThis.endId = (await risuai.risunestOnGenerationEnd((event) => { globalThis.endEvents.push(event); })).id;
            })();
        `)
        const target = { characterId: 'char', conversationId: 'conv' }
        const end = async (status: GenerationEndStatus) => {
            const run = beginGenerationEndRun(target)
            await run.capture(async () => noteGenerationStarted(target))
            run.finish(status)
        }
        const received = async () => JSON.parse(String(await guest(name, 'return JSON.stringify(globalThis.endEvents)')))
        mocks.locateGenerationEnd.mockClear()

        await end('failed')
        await vi.waitFor(async () => expect(await received()).toEqual([{
            status: 'failed', characterId: 'char', conversationId: 'conv',
            characterIndex: 1, chatIndex: 0, messageIndex: 3, messageId: 'gen-1',
        }]))

        await guest(name, 'await risuai.unregisterUIPart(globalThis.endId); return true')
        await end('completed')
        expect(mocks.locateGenerationEnd).toHaveBeenCalledOnce()
        expect(await received()).toHaveLength(1)

        await guest(name, 'await risuai.risunestOnGenerationEnd(() => {}); return true')
        await loadV3Plugins([])
        await end('completed')
        expect(mocks.locateGenerationEnd).toHaveBeenCalledOnce()
    })

    it('does not log RPC request or response payloads when DEV is false', async () => {
        vi.stubEnv('DEV', false)
        installFixtureDatabase()
        mocks.permissionValues.clear()
        mocks.confirm.mockResolvedValue(true)
        const log = vi.spyOn(console, 'log').mockImplementation(() => undefined)
        await executePluginV3({ name: pluginName, script: fixtureScript } as any)
        await guestEvaluation
        await guest(pluginName, 'await globalThis.rpcReady; return true')

        expect(log.mock.calls.map(([first]) => first)).not.toContain('Original request:')
        expect(log.mock.calls.map(([first]) => first)).not.toContain('Original response:')
    })
})
