import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { SandboxHost } from './factory'
import {
    executePluginV3,
    getV3PluginInstance,
    loadV3Plugins,
} from './v3.svelte'

const mocks = vi.hoisted(() => ({
    database: null as any,
    selectedId: 0,
    confirm: vi.fn(async () => true),
    permissionValues: new Map<string, unknown>(),
}))

vi.mock('../plugins.svelte', () => {
    const oldApis = new Proxy({}, { get: () => vi.fn() })
    return {
        allowedDbKeys: [],
        applyPreparedPluginDatabaseUpdate: vi.fn(),
        chatOutputListenerProvenance: new WeakMap(),
        customProviderStore: {
            subscribe: (run: (value: unknown) => void) => { run([]); return () => undefined },
            set: vi.fn(),
        },
        getV2PluginAPIs: () => oldApis,
        handlePluginInstallViaPlugin: vi.fn(),
        pluginCompatibility: { profile: 'scalable-v3' },
        pluginStorageStore: {
            snapshot: vi.fn(), mutate: vi.fn(), invalidate: vi.fn(), getItem: vi.fn(),
            setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(), key: vi.fn(),
            keys: vi.fn(), length: vi.fn(),
        },
        pluginV2: { providers: new Map(), providerOptions: new Map() },
    }
})
vi.mock('src/ts/storage/database.svelte', () => ({ getDatabase: () => mocks.database }))
vi.mock('../pluginSafeClass', () => ({
    SafeLocalPluginStorage: class {},
    tagWhitelist: ['button', 'span', 'div'],
}))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: {
        get db() { return mocks.database },
        set db(value) { mocks.database = value },
    },
    selectedCharID: {
        subscribe(run: (value: number) => void) { run(mocks.selectedId); return () => undefined },
    },
    additionalChatMenu: [], additionalFloatingActionButtons: [], additionalHamburgerMenu: [],
    additionalSettingsMenu: [], bodyIntercepterStore: [], chatPanelStore: [],
}))
vi.mock('src/ts/alert', () => ({
    alertConfirm: mocks.confirm,
    alertError: vi.fn(),
    alertNormal: vi.fn(),
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
vi.mock('src/ts/translator/translator', () => ({
    getLLMCache: vi.fn(), searchLLMCache: vi.fn(),
}))
vi.mock('src/ts/parser/parser.svelte', () => ({ hasher: vi.fn(async () => 'fixture-hash') }))
vi.mock('localforage', () => ({ default: { createInstance: () => ({
    getItem: vi.fn((key: string) => mocks.permissionValues.get(key) ?? null),
    setItem: vi.fn((key: string, value: unknown) => mocks.permissionValues.set(key, value)),
}) } }))
vi.mock('src/ts/process/index.svelte', () => ({
    sendChat: vi.fn(),
    doingChat: {
        subscribe(run: (value: boolean) => void) { run(false); return () => undefined },
    },
}))
vi.mock('src/ts/model/modellist', () => ({ getModelInfo: () => ({ id: 'fixture-model' }) }))
vi.mock('src/ts/process/request/request', () => ({ requestChatDataMain: vi.fn() }))
vi.mock('src/ts/process/modules', () => ({ getModuleLorebooks: vi.fn() }))
vi.mock('src/ts/process/ttsHooks', () => ({
    registerTTSPreprocessor: vi.fn(), unregisterTTSPreprocessor: vi.fn(),
    registerTTSPostprocessor: vi.fn(), unregisterTTSPostprocessor: vi.fn(),
}))
vi.mock('src/ts/storage/persistentDataRuntime.svelte', () => ({
    flushPendingData: vi.fn(), acquireCompleteConversation: vi.fn(),
    captureSelectedConversationTarget: vi.fn(), getActiveConversationSession: vi.fn(),
    getPersistentNavigationGeneration: vi.fn(() => 0),
    invalidateActiveConversationSession: vi.fn(),
    materializePersistentDatabaseSnapshotWithRevision: vi.fn(),
    refreshSelectedConversationAfterReplacement: vi.fn(),
    replacePersistentCompleteCharacter: vi.fn(),
    replacePersistentConversation: vi.fn(), replacePersistentDatabase: vi.fn(),
}))
vi.mock('../pluginCompatibility', () => ({ runPluginFullObjectReplacement: vi.fn() }))
vi.mock('../pluginDatabaseAccess', () => ({
    createProductionPluginDatabaseAccess: vi.fn(() => ({})),
    linkPluginQueryAbortSignals: vi.fn(),
}))
vi.mock('../pluginChatOutputListeners', () => ({
    registerChatOutputListener: vi.fn(), removeChatOutputListener: vi.fn(),
}))
vi.mock('src/ts/conversationMutations', () => ({ appendCurrentConversationMessage: vi.fn() }))

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

describe('Plugin v3 real iframe DOM/RPC bridge', () => {
    beforeEach(() => {
        vi.clearAllMocks()
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
    const data = JSON.parse(JSON.stringify(message));
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
            vi.spyOn(child, 'postMessage').mockImplementation((data) => {
                child.dispatchEvent(new childRealm.MessageEvent('message', {
                    data: structuredClone(data),
                    source: window,
                }))
            })
            const source = sourceValue.match(/<script nonce="[^"]+">([\s\S]*)<\/script>/)?.[1]
            if (!source) throw new Error('Sandbox guest script was not found')
            guestEvaluation = childRealm.eval(source)
            guestEvaluations.push(guestEvaluation)
        })
        mocks.permissionValues.clear()
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
