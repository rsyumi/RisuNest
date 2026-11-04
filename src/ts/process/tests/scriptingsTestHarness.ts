import { vi } from 'vitest'
import { parserStubModule } from '../../parser/parser.testSupport'

// Shared vi.mock module factories for the Lua scripting suites (scriptings*.test.ts).
// Each factory returns the part of the module that scriptings.ts and the Lua host read. Consume as:
//   vi.mock('../alert', async () => (await import('./tests/scriptingsTestHarness')).alertModule())
// Defaults are checked against the module types, and overrides must name exports the module has.

type Overrides<M> = { [K in keyof M]?: unknown }

type PersistentRuntimeModule = typeof import('../../storage/persistentDataRuntime.svelte')
type DatabaseModule = typeof import('../../storage/database.svelte')
type StoresModule = typeof import('../../stores.svelte')
type HypamemoryModule = typeof import('../memory/hypamemory')

/** The persistence runtime with the active conversation session that `session` returns, and no windowed selection. */
export function persistentRuntimeModule(
    session: () => ReturnType<PersistentRuntimeModule['peekActiveConversationSession']> = () => null,
) {
    return {
        peekActiveConversationSession: session,
        captureSelectedConversationAuthority: vi.fn(() => null),
        getActiveConversationViewportSource: vi.fn(() => null),
    } satisfies Partial<PersistentRuntimeModule>
}

export function parserModule() {
    return parserStubModule({ hasher: vi.fn(), risuChatParser: vi.fn() })
}

export function alertModule() {
    return {
        alertConfirm: vi.fn(),
        alertError: vi.fn(),
        alertInput: vi.fn(),
        alertNormal: vi.fn(),
        alertSelect: vi.fn(),
        alertToast: vi.fn(),
    } satisfies Partial<typeof import('../../alert')>
}

export function tokenizerModule() {
    return { tokenize: vi.fn() } satisfies Partial<typeof import('../../tokenizer')>
}

export function utilModule() {
    return {
        asBuffer: vi.fn(),
        getPersonaPrompt: vi.fn(),
        getUserIcon: vi.fn(),
        getUserName: vi.fn(),
        parseKeyValue: vi.fn(() => []),
        pickHashRand: vi.fn(),
    } satisfies Partial<typeof import('../../util')>
}

/** An empty database with an empty current character and chat. */
export function databaseModule(overrides: Overrides<DatabaseModule> = {}) {
    const module = {
        appVer: 'test',
        getCurrentCharacter: vi.fn(() => ({}) as ReturnType<DatabaseModule['getCurrentCharacter']>),
        getCurrentChat: vi.fn(() => ({ message: [] }) as ReturnType<DatabaseModule['getCurrentChat']>),
        getDatabase: vi.fn(() => ({ characters: [] }) as ReturnType<DatabaseModule['getDatabase']>),
        setDatabase: vi.fn(),
    } satisfies Partial<DatabaseModule>
    return { ...module, ...overrides }
}

function storeStub<T>(value: T) {
    return {
        subscribe: (run: (value: T) => void) => {
            run(value)
            return () => {}
        },
        set: vi.fn(),
        update: vi.fn(),
    }
}

/** The stores the Lua host reads, with the character at `selectedIndex` selected. */
export function storesModule(selectedIndex: () => number = () => 0) {
    return {
        DBState: { db: {} } as StoresModule['DBState'],
        ReloadChatPointer: storeStub({}),
        ReloadGUIPointer: storeStub(0),
        ScriptDisplayRefreshPointer: storeStub(0),
        ScriptDisplayRebuildPointer: storeStub(0),
        CurrentTriggerIdStore: storeStub(null),
        selectedCharID: {
            subscribe: (run: (value: number) => void) => (run(selectedIndex()), () => undefined),
        } as StoresModule['selectedCharID'],
    } satisfies Partial<StoresModule>
}

export function modulesModule() {
    return {
        getModuleLorebooks: vi.fn(() => []),
        getModuleTriggers: vi.fn(() => []),
        getModuleRegexScripts: vi.fn(() => []),
        getModuleAssets: vi.fn(() => []),
    } satisfies Partial<typeof import('../modules')>
}

export function inlaysModule() {
    return { getInlayAsset: vi.fn(), writeInlayImage: vi.fn() } satisfies Partial<typeof import('../files/inlays')>
}

export function lorebookModule() {
    return {
        loadLoreBookV3PromptFromCompatibilitySnapshot: vi.fn(),
    } satisfies Partial<typeof import('../lorebook.svelte')>
}

export function hypamemoryModule() {
    return {
        HypaProcesser: vi.fn() as unknown as HypamemoryModule['HypaProcesser'],
    } satisfies Partial<HypamemoryModule>
}

export function requestModule() {
    return { requestChatData: vi.fn() } satisfies Partial<typeof import('../request/request')>
}

export function stableDiffModule() {
    return { generateAIImage: vi.fn() } satisfies Partial<typeof import('../stableDiff')>
}

export function commandModule() {
    return { processMultiCommand: vi.fn() } satisfies Partial<typeof import('../command')>
}
