import { PluginStorageObservation } from '../pluginStorageProtection'
import { vi } from 'vitest'
import { writable } from 'svelte/store'
import type { LLMModel } from 'src/ts/model/types'

// Shared vi.mock module factories for the suites that run the v3 plugin host (v3.svelte.ts)
// with its import graph cut. Each suite keeps its own vi.mock lines and passes its state:
//   vi.mock('src/ts/stores.svelte', async () =>
//       (await import('./v3Host.testSupport')).storesModule({ read: () => mocks.database }))
// Overrides replace single exports and must name exports the module has.

type Overrides<M> = { [K in keyof M]?: unknown }

type PluginsModule = typeof import('../plugins.svelte')
type PluginSafeClassModule = typeof import('../pluginSafeClass')
type FactoryModule = typeof import('./factory')
type PluginDatabaseAccessModule = typeof import('../pluginDatabaseAccess')

/** The database the host reads through `getDatabase` and `DBState`, and the selected character index. */
export type V3HostDatabase = {
    read: () => any
    /** Without it, `DBState.db` cannot be assigned. */
    write?: (database: any) => void
    selectedIndex?: () => number
}

/** One plugin's storage, as `pluginStorageStore.forOwner` returns it. */
export function ownedPluginStorageStub() {
    return {
        observation: new PluginStorageObservation('test-plugin'), dispose: vi.fn(),
        getItem: vi.fn(), setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(),
        key: vi.fn(), keys: vi.fn(), length: vi.fn(), snapshot: vi.fn(async () => ({})),
        mutate: vi.fn(),
    }
}

export function pluginStorageStoreStub(
    owner = 'test-plugin',
    forOwner: (owner: string) => unknown = (() => {
        const owned = ownedPluginStorageStub()
        return () => owned
    })(),
) {
    return {
        forOwner,
        ownerOf: () => owner,
        invalidateOwner: vi.fn(),
        synchronizeCommittedMutation: vi.fn(),
        snapshot: vi.fn(async () => []), mutate: vi.fn(), invalidate: vi.fn(), getItem: vi.fn(),
        setItem: vi.fn(), removeItem: vi.fn(), clear: vi.fn(), key: vi.fn(),
        keys: vi.fn(), length: vi.fn(),
    }
}

/** The v2 API object: members of `apis` first, a fresh mock for every other member. */
export function v2PluginApis(apis: object = {}) {
    return new Proxy(apis, { get: (target, property) => Reflect.get(target, property) ?? vi.fn() })
}

export function pluginsModule(overrides: Overrides<PluginsModule> = {}) {
    const apis = v2PluginApis()
    const module = {
        allowedDbKeys: [],
        applyPreparedPluginDatabaseUpdate: vi.fn(),
        customProviderStore: writable<string[]>([]),
        getV2PluginAPIs: (() => apis) as unknown as PluginsModule['getV2PluginAPIs'],
        handlePluginInstallViaPlugin: vi.fn(),
        pluginStorageStore: pluginStorageStoreStub() as unknown as PluginsModule['pluginStorageStore'],
        pluginV2: { providers: new Map(), providerOptions: new Map(), chatOutput: new Set() } as unknown as PluginsModule['pluginV2'],
    } satisfies Partial<PluginsModule>
    return { ...module, ...overrides }
}

export function databaseModule(host: V3HostDatabase, overrides: Overrides<typeof import('src/ts/storage/database.svelte')> = {}) {
    const module = {
        getDatabase: () => host.read(),
    } satisfies Partial<typeof import('src/ts/storage/database.svelte')>
    return { ...module, ...overrides }
}

export function storesModule(host: V3HostDatabase) {
    const DBState = host.write
        ? { get db() { return host.read() }, set db(value) { host.write!(value) } }
        : { get db() { return host.read() } }
    return {
        DBState,
        selectedCharID: {
            subscribe(run: (value: number) => void) { run(host.selectedIndex?.() ?? 0); return () => undefined },
            set: vi.fn(),
            update: vi.fn(),
        },
        additionalChatMenu: [], additionalFloatingActionButtons: [], additionalHamburgerMenu: [],
        additionalSettingsMenu: [], bodyIntercepterStore: [], chatPanelStore: [],
    } satisfies Partial<typeof import('src/ts/stores.svelte')>
}

export function pluginSafeClassModule(overrides: Overrides<PluginSafeClassModule> = {}) {
    const module = {
        SafeLocalPluginStorage: class {} as unknown as PluginSafeClassModule['SafeLocalPluginStorage'],
        SafeLocalStorage: class {} as unknown as PluginSafeClassModule['SafeLocalStorage'],
        tagWhitelist: [],
    } satisfies Partial<PluginSafeClassModule>
    return { ...module, ...overrides }
}

export function alertModule(overrides: Overrides<typeof import('src/ts/alert')> = {}) {
    const module = {
        alertConfirm: vi.fn(async () => true),
        alertError: vi.fn(),
        alertNormal: vi.fn(),
    } satisfies Partial<typeof import('src/ts/alert')>
    return { ...module, ...overrides }
}

export function utilModule(overrides: Overrides<typeof import('src/ts/util')> = {}) {
    const module = {
        sleep: vi.fn(async () => undefined),
    } satisfies Partial<typeof import('src/ts/util')>
    return { ...module, ...overrides }
}

export function globalApiModule(overrides: Overrides<typeof import('src/ts/globalApi.svelte')> = {}) {
    const module = {
        checkCharOrder: vi.fn(),
        forageStorage: {} as typeof import('src/ts/globalApi.svelte').forageStorage,
        getFetchLogs: vi.fn(),
        downloadFile: vi.fn(async () => true),
    } satisfies Partial<typeof import('src/ts/globalApi.svelte')>
    return { ...module, ...overrides }
}

export function colorschemeModule() {
    return {
        changeColorScheme: vi.fn(), updateColorScheme: vi.fn(), updateTextThemeAndCSS: vi.fn(),
    } satisfies Partial<typeof import('src/ts/gui/colorscheme')>
}

export function pluginMcpModule() {
    return {
        registerMCPModule: vi.fn(), unregisterMCPModule: vi.fn(),
    } satisfies Partial<typeof import('src/ts/process/mcp/pluginmcp')>
}

export function inlaysModule() {
    return { getInlayAsset: vi.fn() } satisfies Partial<typeof import('src/ts/process/files/inlays')>
}

export function translatorModule() {
    return {
        getLLMCache: vi.fn(), searchLLMCache: vi.fn(),
    } satisfies Partial<typeof import('src/ts/translator/translator')>
}

export function processIndexModule(overrides: Overrides<typeof import('src/ts/process/index.svelte')> = {}) {
    const module = {
        sendChat: vi.fn(),
        doingChat: writable(false),
    } satisfies Partial<typeof import('src/ts/process/index.svelte')>
    return { ...module, ...overrides }
}

export function modellistModule() {
    return {
        getModelInfo: () => ({ id: 'test-model' }) as LLMModel,
    } satisfies Partial<typeof import('src/ts/model/modellist')>
}

export function requestModule() {
    return { requestChatDataMain: vi.fn() } satisfies Partial<typeof import('src/ts/process/request/request')>
}

export function modulesModule() {
    return { getModuleLorebooks: vi.fn() } satisfies Partial<typeof import('src/ts/process/modules')>
}

export function ttsHooksModule() {
    return {
        registerTTSPreprocessor: vi.fn(), unregisterTTSPreprocessor: vi.fn(),
        registerTTSPostprocessor: vi.fn(), unregisterTTSPostprocessor: vi.fn(),
    } satisfies Partial<typeof import('src/ts/process/ttsHooks')>
}

export function persistentRuntimeModule(overrides: Overrides<typeof import('src/ts/storage/persistentDataRuntime.svelte')> = {}) {
    const module = {
        getPersistentRevision: () => 0,
        commitPersistentUnitIntent: vi.fn(),
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
    } satisfies Partial<typeof import('src/ts/storage/persistentDataRuntime.svelte')>
    return { ...module, ...overrides }
}

export function pluginDatabaseAccessModule(overrides: Overrides<PluginDatabaseAccessModule> = {}) {
    const module = {
        createProductionPluginDatabaseAccess: vi.fn(() => ({}) as ReturnType<PluginDatabaseAccessModule['createProductionPluginDatabaseAccess']>),
        linkPluginQueryAbortSignals: vi.fn(),
    } satisfies Partial<PluginDatabaseAccessModule>
    return { ...module, ...overrides }
}

/** A sandbox that runs nothing and hands the API it was built with to `onCreate`. */
export function sandboxHostModule(onCreate: (api: any) => void = () => undefined) {
    return {
        SandboxHost: class {
            constructor(api: unknown) { onCreate(api) }
            run() {}
            terminate() {}
            onScriptSettled() {}
        } as unknown as FactoryModule['SandboxHost'],
    } satisfies Partial<FactoryModule>
}
