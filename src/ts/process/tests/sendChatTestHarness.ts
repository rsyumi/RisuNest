import { vi } from 'vitest'

// Shared vi.mock module factories for the sendChat integration suites.
// Each factory returns the module shape expected by index.svelte.ts and its
// dependency graph; suites pass overrides for the pieces they drive from
// their own hoisted state. Consume from a test file as:
//   vi.mock('../tokenizer', async () =>
//       (await import('./tests/sendChatTestHarness')).tokenizerModule({ ... }))

type ModuleOverrides = Record<string, unknown>

export function tokenizerModule(overrides: ModuleOverrides & {
    tokenizeChat?: (...args: unknown[]) => Promise<number> | number
    tokenizeChats?: (chats: unknown[]) => Promise<number> | number
} = {}) {
    const { tokenizeChat, tokenizeChats, ...rest } = overrides
    const chatTokenizer = tokenizeChat ?? (async () => 1)
    const chatsTokenizer = tokenizeChats ?? (async (chats: unknown[]) => chats.length)
    return {
        ChatTokenizer: class {
            tokenizeChat(...args: unknown[]) {
                return chatTokenizer(...args)
            }
            tokenizeChats(chats: unknown[]) {
                return chatsTokenizer(chats)
            }
        },
        tokenize: vi.fn(async () => 1),
        tokenizeNum: vi.fn(async () => []),
        ...rest,
    }
}

export function langModule(language: ModuleOverrides = {}) {
    return {
        changeLanguage: vi.fn(),
        language: {
            errors: { toomuchtoken: 'too many tokens', httpError: 'http error' },
            otherUserRequesting: 'other user requesting',
            ...language,
        },
    }
}

export function alertModule(overrides: ModuleOverrides = {}) {
    return { alertError: vi.fn(), alertToast: vi.fn(), ...overrides }
}

export function chatMLModule(overrides: ModuleOverrides = {}) {
    return { parseChatML: (value: string) => value, ...overrides }
}

export function parserModule(overrides: ModuleOverrides = {}) {
    return { risuChatParser: (value: string) => value, ...overrides }
}

export function lorebookModule(overrides: ModuleOverrides = {}) {
    return { loadLoreBookV3Prompt: vi.fn(async () => ({ actives: [] })), ...overrides }
}

export function utilModule(overrides: ModuleOverrides = {}) {
    return {
        checkNullish: (value: unknown) => value === null || value === undefined,
        decryptBuffer: vi.fn(),
        encryptBuffer: vi.fn(),
        selectSingleFile: vi.fn(),
        findCharacterbyId: vi.fn(),
        getAuthorNoteDefaultText: () => '',
        getPersonaPrompt: () => '',
        getUserName: () => 'User',
        isLastCharPunctuation: () => true,
        trimUntilPunctuation: (value: string) => value,
        parseToggleSyntax: () => [],
        parseKeyValue: () => [],
        sleep: async () => undefined,
        prebuiltAssetCommand: '',
        ...overrides,
    }
}

export function scriptsModule(overrides: ModuleOverrides = {}) {
    return {
        createPromptScriptOperationScope: () => ({
            assertOwnerCurrent: vi.fn(),
            adoptMessageId: vi.fn(),
            parse: (_char: unknown, text: string) => text,
            finish: vi.fn(),
            finishAfterError: vi.fn(),
            release: vi.fn(),
        }),
        processScript: vi.fn(async (_char: unknown, data: string) => data),
        processScriptFull: vi.fn(async (_char: unknown, data: string) => ({
            data,
            emoChanged: false,
        })),
        risuChatParser: (value: string) => value,
        resetScriptCache: vi.fn(),
        ...overrides,
    }
}

export function templatesModule(overrides: ModuleOverrides = {}) {
    return {
        prebuiltNAIpresets: [],
        prebuiltPresets: { OAI: { mainPrompt: '', jailbreak: '' } },
        ...overrides,
    }
}

export function exampleMessagesModule(overrides: ModuleOverrides = {}) {
    return { exampleMessage: vi.fn(() => []), ...overrides }
}

export function ttsModule(overrides: ModuleOverrides = {}) {
    return { sayTTS: vi.fn(async () => undefined), ...overrides }
}

export function stableDiffModule(overrides: ModuleOverrides = {}) {
    return { stableDiff: vi.fn(), ...overrides }
}

export function groupModule(overrides: ModuleOverrides = {}) {
    return { groupOrder: vi.fn((value: unknown) => value), ...overrides }
}

export function supaMemoryModule(overrides: ModuleOverrides = {}) {
    return { supaMemory: vi.fn(), ...overrides }
}

export function hypamemoryModule(overrides: ModuleOverrides = {}) {
    return { HypaProcesser: class {}, ...overrides }
}

export function hanuraiMemoryModule(overrides: ModuleOverrides = {}) {
    return { hanuraiMemory: vi.fn(), ...overrides }
}

export function hypav2Module(overrides: ModuleOverrides = {}) {
    return { hypaMemoryV2: vi.fn(), ...overrides }
}

export function hypav3Module(overrides: ModuleOverrides = {}) {
    return { hypaMemoryV3: vi.fn(), ...overrides }
}

export function addinfoModule(overrides: ModuleOverrides = {}) {
    return { additionalInformations: vi.fn(async () => ''), ...overrides }
}

export function inlaysModule(overrides: ModuleOverrides = {}) {
    return { getInlayAsset: vi.fn(async () => null), ...overrides }
}

export function modelStringModule(overrides: ModuleOverrides = {}) {
    return { getGenerationModelString: vi.fn(() => 'test-model'), ...overrides }
}

export function multiuserModule(overrides: ModuleOverrides = {}) {
    return {
        connectionOpen: false,
        peerRevertChat: vi.fn(),
        peerSafeCheck: vi.fn(async () => true),
        peerSync: vi.fn(async () => undefined),
        ...overrides,
    }
}

export function inlayScreenModule(overrides: ModuleOverrides = {}) {
    return {
        runInlayScreen: vi.fn((_char: unknown, data: string) => ({ text: data })),
        ...overrides,
    }
}

export function transformersModule(overrides: ModuleOverrides = {}) {
    return { runImageEmbedding: vi.fn(), ...overrides }
}

export function scriptingsModule(overrides: ModuleOverrides = {}) {
    return {
        runLuaEditTrigger: vi.fn(async (_char: unknown, _mode: string, value: unknown) => value),
        ...overrides,
    }
}

export function modellistModule(overrides: ModuleOverrides = {}) {
    return {
        getModelInfo: vi.fn(() => ({ flags: [] })),
        LLMFlags: { hasImageInput: 'hasImageInput' },
        ...overrides,
    }
}

export function modulesModule(overrides: ModuleOverrides = {}) {
    return {
        getModuleAssets: vi.fn(() => []),
        getModuleToggles: vi.fn(() => ''),
        moduleUpdate: vi.fn(),
        ...overrides,
    }
}

export function globalApiModule(overrides: ModuleOverrides = {}) {
    return { readImage: vi.fn(), ...overrides }
}

export function pluginsModule(chatOutput: Set<unknown> = new Set()) {
    return {
        pluginV2: { chatOutput },
        chatOutputListenerProvenance: new WeakMap(),
        pluginCompatibility: { profile: 'maximum-compatibility' },
    }
}

export function presetChainModule(overrides: ModuleOverrides = {}) {
    return { activatePresetChainForRequest: vi.fn(async () => undefined), ...overrides }
}
