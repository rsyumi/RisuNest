import { zlibSync } from 'fflate'

import type { Database } from '../../database.svelte'

function makePluginStorage(): Database['pluginCustomStorage'] {
    const storage = Object.create(null) as Database['pluginCustomStorage']
    storage['10'] = 'ten'
    storage['2'] = 'two'
    storage['01'] = 0
    storage['4294967294'] = { ordered: ['first', 'first', 'second'] }
    storage['4294967295'] = false
    storage.__proto__ = { empty: '', nestedUnknown: { enabled: false } }
    storage[''] = null
    storage['unicode-한국어'] = { unknown: {}, emptyList: [] }
    return storage
}

const completeMessage = {
    role: 'user',
    data: 'complete {{inlay::inlay-image}} and duplicate {{inlay::inlay-image}}',
    saying: '',
    chatId: 'message-complete',
    time: 1_700_000_000_000,
    generationInfo: {
        model: 'fixture-model',
        generationId: 'generation-zero',
        inputTokens: 0,
        outputTokens: 0,
        maxContext: 0,
        stageTiming: { stage1: 0, stage2: 2, stage3: 3, stage4: 4 },
        roadmap14Unknown: { empty: {} },
    },
    promptInfo: {
        promptName: 'Prompt Zeta',
        promptToggles: [{ key: 'empty', value: '' }],
        promptText: [
            { role: 'system', content: '' },
            { role: 'user', content: 'fixture prompt' },
        ],
        roadmap14Unknown: false,
    },
    name: '',
    otherUser: false,
    disabled: false,
    isComment: false,
    roadmap14Unknown: { nullable: null, absentSibling: undefined },
}

const completeChat = {
    message: [
        completeMessage,
        {
            role: 'char',
            data: '{{inlay::inlay-audio}} {{inlay::inlay-video}} {{inlay::inlay-signature}}',
            chatId: 'message-inlays',
            time: 1_700_000_000_001,
        },
    ],
    note: '',
    name: 'Complete chat',
    localLore: [
        {
            key: 'chat-lore',
            secondkey: '',
            insertorder: 0,
            comment: '',
            content: 'Local lore',
            mode: 'normal',
            alwaysActive: false,
            selective: false,
            useRegex: false,
            roadmap14Unknown: [],
        },
    ],
    sdData: '',
    supaMemoryData: '',
    hypaV2Data: { chunks: [], roadmap14Unknown: false },
    lastMemory: '',
    suggestMessages: [],
    isStreaming: false,
    activeStreamingDisplayOptimizationMode: 'off',
    scriptstate: { zero: 0, disabled: false, empty: '' },
    modules: ['module-main', 'module-main'],
    id: 'chat-complete',
    bindedPersona: 'persona-main',
    fmIndex: 0,
    hypaV3Data: { summaries: [], roadmap14Unknown: null },
    folderId: 'chat-folder-main',
    lastDate: 1_700_000_000_002,
    bookmarks: ['message-complete', 'message-complete'],
    bookmarkNames: { 'message-complete': '', roadmap14Unknown: 'bookmark-extension' },
    useLocallySetGlobalVariables: false,
    GLGlobalVariables: { empty: '', zero: '0' },
    roadmap14Unknown: { nested: { emptyObject: {}, emptyArray: [] } },
}

const normalCharacter = {
    type: 'character',
    name: 'Compatibility Character',
    image: 'assets/characters/main.PNG',
    firstMessage: 'Hello {{inlay::inlay-image}}',
    desc: 'Synthetic normal character',
    notes: '',
    chats: [
        completeChat,
        {
            id: 'chat-cold-pointer',
            name: 'Cold pointer chat',
            note: '',
            localLore: [],
            message: [{ role: 'user', data: '\uEF01COLDSTORAGE\uEF01cold-chat-main' }],
        },
    ],
    chatFolders: [
        { id: 'chat-folder-main', name: 'Folder Zeta', color: '', folded: false },
        { id: 'chat-folder-empty', name: '', color: '', folded: true },
    ],
    chatPage: 0,
    viewScreen: 'none',
    bias: [['duplicate', 0], ['duplicate', 0]],
    emotionImages: [
        ['neutral', 'assets/characters/emotion.webp'],
        ['neutral duplicate', 'assets/characters/emotion.webp'],
    ],
    globalLore: [
        {
            key: 'lore-key',
            secondkey: 'secondary',
            insertorder: 0,
            comment: '',
            content: 'Lore content',
            mode: 'normal',
            alwaysActive: false,
            selective: true,
            extentions: { risu_case_sensitive: false },
            activationPercent: 0,
            loreCache: { key: '', data: [] },
            useRegex: true,
            bookVersion: 0,
            id: 'lore-main',
            folder: '',
            roadmap14Unknown: { ordered: ['zeta', 'alpha'] },
        },
    ],
    chaId: 'character-main',
    sdData: [['seed', '0']],
    customscript: [
        { comment: '', in: '(fixture)', out: '$1', type: 'editoutput', flag: 'g', ableFlag: false },
    ],
    triggerscript: [
        {
            name: 'Fixture trigger',
            type: 'output',
            conditions: [],
            effects: [],
            lowLevelAccess: false,
            roadmap14Unknown: { value: 0 },
        },
    ],
    utilityBot: false,
    exampleMessage: '',
    creatorNotes: '',
    systemPrompt: '',
    postHistoryInstructions: '',
    alternateGreetings: ['', 'Alternate {{inlay::inlay-image}}'],
    tags: ['fixture', 'fixture'],
    creator: 'RisuNest',
    characterVersion: '14',
    personality: '',
    scenario: '',
    firstMsgIndex: 0,
    additionalAssets: [
        ['Shared asset', 'assets/shared/shared.bin', 'bin'],
        ['Shared asset duplicate', 'assets/shared/shared.bin', 'bin'],
        ['Known missing asset', 'assets/missing/known-missing.dat', 'dat'],
    ],
    ccAssets: [
        { type: 'icon', uri: 'assets/cards/card-main.avif', name: 'Card asset', ext: 'avif' },
        { type: 'external', uri: 'https://example.invalid/external.png', name: 'External', ext: 'png' },
    ],
    vits: {
        files: {
            model: 'assets/models/voice.onnx',
            config: 'assets/data/config.json',
        },
    },
    replaceGlobalNote: '',
    additionalText: '',
    virtualscript: 'return "lua fixture"',
    scriptstate: { zero: 0, disabled: false, empty: '' },
    extentions: {
        roadmap14Unknown: {
            nestedInlay: '{{inlay::inlay-known-missing}}',
            ordered: { zeta: 1, alpha: 2 },
        },
    },
    modules: ['module-main', 'module-main'],
    moduleNamespace: 'fixture.namespace',
    coldstorage: 'cold-character-main',
    coldStoragedChats: ['cold-chat-main', 'cold-chat-main'],
    roadmap14Unknown: { falseValue: false, zeroValue: 0, emptyString: '' },
}

const groupCharacter = {
    type: 'group',
    image: 'assets/groups/group-main.gif',
    firstMessage: 'Group greeting',
    chats: [],
    chatFolders: [],
    chatPage: 0,
    name: 'Compatibility Group',
    viewScreen: 'multiple',
    characters: ['character-main', 'character-main', 'character-known-missing'],
    characterTalks: [0, 0, 0],
    characterActive: [true, true, false],
    globalLore: [],
    autoMode: false,
    useCharacterLore: true,
    emotionImages: [['group', 'assets/groups/group-emotion.svg']],
    customscript: [],
    chaId: 'group-main',
    alternateGreetings: [],
    firstMsgIndex: 0,
    realmId: '',
    modules: ['module-secondary'],
    roadmap14Unknown: { empty: {} },
}

const modules = [
    {
        id: 'module-main',
        name: 'Module Zeta',
        description: 'Rich module fixture',
        lorebook: normalCharacter.globalLore,
        regex: normalCharacter.customscript,
        cjs: 'return { value: false }',
        trigger: normalCharacter.triggerscript,
        lowLevelAccess: false,
        hideIcon: false,
        backgroundEmbedding: '',
        assets: [
            ['Module shared', 'assets/shared/shared.bin', 'bin'],
            ['Module shared duplicate', 'assets/shared/shared.bin', 'bin'],
        ],
        namespace: 'fixture.module.main',
        customModuleToggle: '',
        icon: 'assets/modules/module-icon.webp',
        roadmap14Unknown: { opaque: [false, 0, '', null] },
    },
    {
        id: 'module-secondary',
        name: 'Module Alpha',
        description: '',
        assets: [['Known missing', 'assets/missing/module.dat', 'dat']],
        icon: '',
    },
]

const personas = [
    {
        id: 'persona-main',
        name: 'Persona Zeta',
        personaPrompt: '',
        icon: 'assets/personas/persona-main.jpg',
        largePortrait: false,
        note: '',
        embeddedModule: {
            id: 'module-embedded',
            name: 'Embedded module',
            description: '',
            icon: 'assets/modules/embedded-icon.webp',
            assets: [
                ['Embedded asset', 'assets/personas/embedded.WEBP', 'WEBP'],
                ['Embedded duplicate', 'assets/personas/embedded.WEBP', 'WEBP'],
            ],
            cjs: 'return 0',
            roadmap14Unknown: { emptyArray: [] },
        },
        roadmap14Unknown: { falseValue: false },
    },
    {
        id: 'persona-secondary',
        name: 'Persona Alpha',
        personaPrompt: 'Secondary',
        icon: '',
    },
]

const botPresets = [
    {
        name: 'Preset Zeta',
        mainPrompt: '',
        jailbreak: '',
        globalNote: '',
        temperature: 0,
        maxContext: 0,
        maxResponse: 0,
        frequencyPenalty: 0,
        PresensePenalty: 0,
        formatingOrder: ['main', 'chats', 'main'],
        promptPreprocess: false,
        bias: [['duplicate', 0], ['duplicate', 0]],
        ooba: {},
        ainconfig: {},
        promptTemplate: [
            { type: 'plain', name: 'Prompt Zeta', role: 'system', content: '' },
            { type: 'plain', name: 'Prompt Alpha', role: 'user', content: 'alpha' },
        ],
        regex: normalCharacter.customscript,
        image: 'assets/presets/preset-main.png',
        roadmap14Unknown: { nested: { undefinedValue: undefined } },
    },
    {
        name: 'Preset Alpha',
        mainPrompt: 'alpha',
        jailbreak: '',
        globalNote: '',
        temperature: 1,
        maxContext: 4096,
        maxResponse: 512,
        frequencyPenalty: 0,
        PresensePenalty: 0,
        formatingOrder: ['chats', 'main'],
        promptPreprocess: true,
        bias: [],
        ooba: {},
        ainconfig: {},
        promptTemplate: [],
    },
]

const database = {
    apiType: 'roadmap14-fixture',
    username: 'Fixture User',
    formatversion: 4,
    characters: [normalCharacter, groupCharacter],
    botPresets,
    botPresetsId: 0,
    modules,
    enabledModules: ['module-main', 'module-main'],
    personas,
    selectedPersona: 0,
    personaPrompt: '',
    userIcon: 'assets/root/user-icon.png',
    customBackground: 'assets/root/background.avif',
    characterOrder: [
        {
            id: 'folder-main',
            name: 'Folder Zeta',
            data: ['character-main', 'character-main'],
            color: '',
            img: 'data:image/png;base64,AA==',
            imgFile: 'assets/folders/folder-main.svg',
            roadmap14Unknown: { nestedInlay: '{{inlay::inlay-audio}}' },
        },
        'group-main',
        'character-main',
    ],
    loadouts: [
        {
            id: 'loadout-main',
            name: 'Loadout Zeta',
            lastUsed: 0,
            favorite: false,
            characterIds: ['character-main', 'character-main'],
            modules: ['module-main', 'module-main'],
            globalVariables: { empty: '', zero: '0' },
            presetName: 'Preset Zeta',
            personaId: 'persona-main',
            icons: ['assets/characters/main.PNG', 'assets/characters/main.PNG'],
            roadmap14Unknown: { nestedInlay: '{{inlay::inlay-signature}}' },
        },
        {
            id: 'loadout-secondary',
            name: 'Loadout Alpha',
            lastUsed: 0,
            favorite: false,
            characterIds: [],
            modules: [],
            globalVariables: {},
            presetName: 'Preset Alpha',
            personaId: 'persona-secondary',
            icons: [],
        },
    ],
    lastLoadedLoadoutName: 'Loadout Zeta',
    plugins: [
        {
            name: 'Plugin metadata fixture',
            version: '0',
            script: '',
            roadmap14Unknown: { enabled: false },
        },
    ],
    pluginV2: [
        {
            name: 'Plugin v2 fixture',
            version: '0',
            script: '',
            roadmap14Unknown: { empty: {} },
        },
    ],
    pluginCustomStorage: makePluginStorage(),
    loreBook: [
        { name: 'Global lore', data: normalCharacter.globalLore },
    ],
    globalscript: normalCharacter.customscript,
    presetRegex: normalCharacter.customscript,
    promptTemplate: botPresets[0].promptTemplate,
    globalChatVariables: { empty: '', zero: '0', disabled: 'false' },
    roadmap14Unknown: { emptyObject: {}, emptyArray: [], undefinedValue: undefined },
} as unknown as Database

export type Roadmap14PayloadKind = 'asset' | 'inlay' | 'cold'

export type Roadmap14PayloadCategory =
    | 'png'
    | 'jpeg'
    | 'webp'
    | 'gif'
    | 'svg'
    | 'avif'
    | 'unknown-image'
    | 'mp3'
    | 'wav'
    | 'ogg'
    | 'mp4'
    | 'webm'
    | 'json'
    | 'binary'
    | 'onnx'
    | 'extensionless'
    | 'uppercase-extension'
    | 'unknown-extension'
    | 'zero-length'
    | 'non-utf8'
    | 'inlay-image'
    | 'inlay-audio'
    | 'inlay-video'
    | 'inlay-signature'
    | 'cold-character'
    | 'cold-chat'

export interface Roadmap14Payload {
    kind: Roadmap14PayloadKind
    category: Roadmap14PayloadCategory
    key: string
    bytes: Uint8Array
    metadata: {
        name: string
        ext: string
        mime: string
        inlayType?: 'image' | 'audio' | 'video' | 'signature'
    }
}

function fixtureBytes(label: string, prefix: readonly number[] = []): Uint8Array {
    const suffix = new TextEncoder().encode(`risunest-roadmap14:${label}`)
    const bytes = new Uint8Array(prefix.length + suffix.byteLength)
    bytes.set(prefix)
    bytes.set(suffix, prefix.length)
    return bytes
}

function payload(
    category: Roadmap14PayloadCategory,
    key: string,
    mime: string,
    prefix: readonly number[] = [],
): Roadmap14Payload {
    const name = key.split('/').at(-1) ?? key
    const dot = name.lastIndexOf('.')
    return {
        kind: 'asset',
        category,
        key,
        bytes: fixtureBytes(key, prefix),
        metadata: {
            name,
            ext: dot === -1 ? '' : name.slice(dot + 1),
            mime,
        },
    }
}

function inlayPayload(
    category: Roadmap14PayloadCategory,
    key: string,
    inlayType: 'image' | 'audio' | 'video' | 'signature',
    mime: string,
    ext: string,
): Roadmap14Payload {
    return {
        kind: 'inlay',
        category,
        key,
        bytes: fixtureBytes(key),
        metadata: { name: `${key}.${ext}`, ext, mime, inlayType },
    }
}

const coldCharacterValue = {
    character: {
        ...normalCharacter,
        image: 'assets/characters/main.PNG',
        additionalAssets: [
            ['Cold shared', 'assets/shared/shared.bin', 'bin'],
            ['Cold shared duplicate', 'assets/shared/shared.bin', 'bin'],
        ],
        chats: [],
        roadmap14ColdUnknown: '{{inlay::inlay-signature}}',
    },
}

const coldChatValue = {
    message: [
        {
            role: 'char',
            data: 'Cold chat {{inlay::inlay-audio}} {{inlay::inlay-audio}}',
            chatId: 'cold-message-main',
        },
    ],
    roadmap14Unknown: { empty: {}, falseValue: false },
}

export const roadmap14Payloads: Roadmap14Payload[] = [
    payload('uppercase-extension', 'assets/characters/main.PNG', 'image/png', [137, 80, 78, 71]),
    payload('webp', 'assets/characters/emotion.webp', 'image/webp', [82, 73, 70, 70]),
    payload('binary', 'assets/shared/shared.bin', 'application/octet-stream', [0, 255, 1, 254]),
    payload('avif', 'assets/cards/card-main.avif', 'image/avif', [0, 0, 0, 24]),
    payload('onnx', 'assets/models/voice.onnx', 'application/octet-stream', [8, 1, 18, 0]),
    payload('json', 'assets/data/config.json', 'application/json', [123, 125, 10]),
    payload('gif', 'assets/groups/group-main.gif', 'image/gif', [71, 73, 70, 56, 57, 97]),
    payload('svg', 'assets/groups/group-emotion.svg', 'image/svg+xml', [60, 115, 118, 103, 62]),
    payload('webp', 'assets/modules/module-icon.webp', 'image/webp', [82, 73, 70, 70]),
    payload('jpeg', 'assets/personas/persona-main.jpg', 'image/jpeg', [255, 216, 255, 224]),
    payload('webp', 'assets/modules/embedded-icon.webp', 'image/webp', [82, 73, 70, 70]),
    payload('webp', 'assets/personas/embedded.WEBP', 'image/webp', [82, 73, 70, 70]),
    payload('png', 'assets/presets/preset-main.png', 'image/png', [137, 80, 78, 71]),
    payload('png', 'assets/root/user-icon.png', 'image/png', [137, 80, 78, 71]),
    payload('avif', 'assets/root/background.avif', 'image/avif', [0, 0, 0, 24]),
    payload('svg', 'assets/folders/folder-main.svg', 'image/svg+xml', [60, 115, 118, 103, 62]),
    payload('unknown-image', 'assets/images/opaque.xyzimg', 'application/x-image-fixture'),
    payload('mp3', 'assets/audio/sample.mp3', 'audio/mpeg', [73, 68, 51]),
    payload('wav', 'assets/audio/sample.wav', 'audio/wav', [82, 73, 70, 70]),
    payload('ogg', 'assets/audio/sample.ogg', 'audio/ogg', [79, 103, 103, 83]),
    payload('mp4', 'assets/video/sample.mp4', 'video/mp4', [0, 0, 0, 24]),
    payload('webm', 'assets/video/sample.webm', 'video/webm', [26, 69, 223, 163]),
    payload('extensionless', 'assets/misc/extensionless', 'application/octet-stream'),
    payload('unknown-extension', 'assets/misc/unknown.weird', 'application/x-risunest-fixture'),
    {
        ...payload('zero-length', 'assets/misc/zero.dat', 'application/octet-stream'),
        bytes: new Uint8Array(0),
    },
    {
        ...payload('non-utf8', 'assets/misc/non-utf8.bin', 'application/octet-stream'),
        bytes: new Uint8Array([0, 255, 254, 128, 195, 40]),
    },
    inlayPayload('inlay-image', 'inlay-image', 'image', 'image/webp', 'webp'),
    inlayPayload('inlay-audio', 'inlay-audio', 'audio', 'audio/ogg', 'ogg'),
    inlayPayload('inlay-video', 'inlay-video', 'video', 'video/webm', 'webm'),
    inlayPayload('inlay-signature', 'inlay-signature', 'signature', 'application/octet-stream', 'bin'),
    {
        kind: 'cold',
        category: 'cold-character',
        key: 'cold-character-main',
        bytes: zlibSync(
            new TextEncoder().encode(JSON.stringify(coldCharacterValue)),
        ),
        metadata: { name: 'cold-character-main.json', ext: 'json', mime: 'application/json' },
    },
    {
        kind: 'cold',
        category: 'cold-chat',
        key: 'cold-chat-main',
        bytes: zlibSync(
            new TextEncoder().encode(JSON.stringify(coldChatValue)),
        ),
        metadata: { name: 'cold-chat-main.json', ext: 'json', mime: 'application/json' },
    },
]

export interface Roadmap14CardFixture {
    id: string
    characterId: string
    moduleId: unknown
    personaId: string
    folderId: string
    assetKeys: [string, string, string][]
    relatedCardIds: string[]
    metadata: unknown
}

export const roadmap14Cards: Roadmap14CardFixture[] = [
    {
        id: 'card-main',
        characterId: 'character-main',
        moduleId: 'module-main',
        personaId: 'persona-main',
        folderId: 'folder-main',
        assetKeys: [
            ['Shared card asset', 'assets/shared/shared.bin', 'bin'],
            ['Shared card asset duplicate', 'assets/shared/shared.bin', 'bin'],
        ],
        relatedCardIds: ['card-secondary', 'card-secondary', 'card-known-missing'],
        metadata: { unknown: { inlay: '{{inlay::inlay-video}}' } },
    },
    {
        id: 'card-secondary',
        characterId: 'group-main',
        moduleId: '',
        personaId: 'persona-secondary',
        folderId: 'folder-main',
        assetKeys: [['Card payload', 'assets/cards/card-main.avif', 'avif']],
        relatedCardIds: [],
        metadata: { emptyObject: {}, emptyArray: [], falseValue: false },
    },
]

export const roadmap14ExpectedMissing = {
    asset: [
        'assets/missing/known-missing.dat',
        'assets/missing/module.dat',
    ],
    inlay: ['inlay-known-missing'],
    character: ['character-known-missing'],
    card: ['card-known-missing'],
} as const

export const roadmap14Corpus = {
    version: 1,
    database,
    coldPayloads: roadmap14Payloads.filter((value) => value.kind === 'cold'),
}
