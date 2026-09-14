import { writable } from 'svelte/store'
import { beforeEach, expect, test, vi } from 'vitest'
import type { Database, Message, character, customscript } from '../../../storage/database.svelte'
import type { ChatScreenshotRenderContext } from '../../../chatScreenshotRange'

const live = vi.hoisted(() => ({
    database: {} as Database,
    modules: [] as Array<{ id: string; name: string; namespace?: string }>,
}))
const bergamotTranslate = vi.hoisted(() => vi.fn(async (
    _text: string,
    from: string,
    to: string,
) => `${from}->${to}`))

vi.mock('../../../storage/database.svelte', () => ({
    appVer: '1.0.0',
    getDatabase: () => live.database,
    getCurrentCharacter: () => live.database.characters[0],
    getCurrentChat: () => live.database.characters[0]?.chats[0],
}))
vi.mock('../../../stores.svelte', () => ({
    DBState: { get db() { return live.database } },
    selIdState: { selId: 0 },
    selectedCharID: writable(0),
    CharEmotion: writable({}),
    CurrentTriggerIdStore: writable(null),
}))
vi.mock('../../../globalApi.svelte', () => ({
    aiWatermarkingLawApplies: () => false,
    downloadFile: vi.fn(),
    globalFetch: vi.fn(),
    getFileSrc: async (source: string) => source,
}))
vi.mock('../../../process/modules', () => ({
    getModules: () => live.modules,
    getModuleLorebooks: () => [],
    getModuleAssets: () => [],
    getModuleRegexScripts: () => [],
}))
vi.mock('../../../process/memory/hypamemory', () => ({ HypaProcesser: class {} }))
vi.mock('../../../process/scriptings', () => ({ runLuaEditTrigger: vi.fn() }))
vi.mock('../../../plugins/plugins.svelte', () => ({
    pluginV2: { editinput: new Set(), editoutput: new Set(), editprocess: new Set(), editdisplay: new Set() },
}))
vi.mock('../../../process/triggers', () => ({ runTrigger: vi.fn() }))
vi.mock('../../../alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('../../../process/index.svelte', () => ({ doingChat: writable(false) }))
vi.mock('../../../process/request/request', () => ({ requestChatData: vi.fn() }))
vi.mock('../../../translator/bergamotTranslator', () => ({ bergamotTranslate }))

const { ParseMarkdown, risuChatParser, trimMarkdown } = await import('../../parser.svelte')
const { processScriptFull } = await import('../../../process/scripts')
const {
    createChatScreenshotDialogSnapshot,
    createChatScreenshotJobFromDialogSnapshot,
    snapshotChatScreenshotCharacter,
} = await import('../../../chatScreenshotRange')
const { clearLLMCache, setLLMCache, translate, translateHTML } = await import('../../../translator/translator')
const { runLuaEditTrigger } = await import('../../../process/scriptings')

function makeCharacter(name: string, messages = [
    { role: 'user' as const, data: `${name} previous` },
    { role: 'char' as const, data: `${name} current` },
]): character {
    return {
        type: 'character',
        name,
        nickname: name,
        chaId: name.toLocaleLowerCase(),
        firstMessage: `${name} first`,
        alternateGreetings: [],
        chats: [{
            message: messages,
            note: '',
            name: '',
            localLore: [],
            fmIndex: -1,
            scriptstate: { $score: `${name} score` },
        }],
        chatPage: 0,
        customscript: [],
        globalLore: [],
        personality: '',
        desc: '',
        scenario: '',
        exampleMessage: '',
        defaultVariables: '',
    } as unknown as character
}

function makeDatabase(character: character, prefix: string): Database {
    return {
        characters: [character],
        username: `${prefix} user`,
        personaPrompt: `${prefix} persona`,
        mainPrompt: `${prefix} main`,
        globalChatVariables: { world: `${prefix} world` },
    } as unknown as Database
}

async function createProductScreenshotJob(input: {
    characterId: string
    chatId: string
    messages: Message[]
    start: number
    end: number
    renderContext: ChatScreenshotRenderContext
}) {
    const parserContext = input.renderContext.parserContext
    const sourceCharacter = parserContext.character
    const sourceChat = sourceCharacter.chats[sourceCharacter.chatPage]
    const captureCharacter = snapshotChatScreenshotCharacter(sourceCharacter, sourceChat)
    const renderContext: ChatScreenshotRenderContext = {
        ...input.renderContext,
        parserContext: {
            ...parserContext,
            character: captureCharacter,
            database: {
                ...parserContext.database,
                characters: parserContext.database.characters.map((candidate, index) =>
                    index === parserContext.selectedCharID ? captureCharacter : candidate,
                ),
            },
        },
    }
    const frozenMessages = structuredClone(input.messages)
    const reader = {
        characterId: input.characterId,
        chatId: input.chatId,
        revision: 1,
        totalTurns: frozenMessages.length,
        async readRange(startIndex: number, limit: number) {
            return structuredClone(frozenMessages.slice(startIndex, startIndex + limit))
        },
    }
    const snapshot = createChatScreenshotDialogSnapshot({
        characterId: reader.characterId,
        chatId: reader.chatId,
        revision: reader.revision,
        sessionVersion: 1,
        totalTurns: reader.totalTurns,
        renderContext,
    })
    return createChatScreenshotJobFromDialogSnapshot(
        snapshot,
        reader,
        input.start,
        input.end,
    )
}

beforeEach(() => {
    live.database = makeDatabase(makeCharacter('Live'), 'Live')
    live.modules = [{ id: 'live', name: 'Live', namespace: 'live-module' }]
})

test('does not rerun live display Lua after an obsolete translation finishes', async () => {
    live.database = {
        ...makeDatabase(makeCharacter('Translation owner'), 'Translation owner'),
        translatorType: 'bergamot',
        translator: 'ko',
        aiModel: 'gpt',
        htmlTranslation: false,
        combineTranslation: true,
    } as Database
    let finishTranslation!: (value: string) => void
    bergamotTranslate.mockImplementationOnce(
        () =>
            new Promise((resolve) => {
                finishTranslation = resolve
            }),
    )
    vi.mocked(runLuaEditTrigger).mockClear()
    const controller = new AbortController()
    const pending = translateHTML(
        '<p>synthetic delayed translation fixture</p>',
        false,
        live.database.characters[0],
        0,
        false,
        undefined,
        controller.signal,
    )
    await vi.waitFor(() => expect(finishTranslation).toBeTypeOf('function'))
    finishTranslation('synthetic translated fixture')
    live.database = makeDatabase(makeCharacter('Replacement owner'), 'Replacement owner')
    controller.abort()
    await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
    expect(runLuaEditTrigger).not.toHaveBeenCalled()
})

test('uses frozen CBS values after the live database, persona, variables, and modules change', () => {
    const frozenCharacter = makeCharacter('Frozen')
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const parserContext = {
        db: frozenDatabase,
        chara: frozenCharacter,
        chatID: 1,
        userName: 'Frozen user',
        personaPrompt: 'Frozen persona',
        modules: [{ id: 'frozen', name: 'Frozen', description: '', namespace: 'frozen-module' }],
        moduleLorebooks: [],
        selectedCharID: 0,
        chatVariables: { score: 'Frozen score' },
        globalChatVariables: { world: 'Frozen world' },
        currentTime: Date.UTC(2020, 0, 2, 3, 4, 5),
    }

    live.database = makeDatabase(makeCharacter('Changed'), 'Changed')
    live.modules = [{ id: 'changed', name: 'Changed', namespace: 'changed-module' }]

    expect(risuChatParser(
        '{{user}}|{{char}}|{{persona}}|{{previoususerchat}}|{{getvar::score}}|{{getglobalvar::world}}|{{moduleenabled::frozen-module}}|{{mainprompt}}|{{date::YYYY-MM-DD}}',
        parserContext,
    )).toBe('Frozen user|Frozen|Frozen persona|Frozen previous|Frozen score|Frozen world|1|Frozen main|2020-01-02')
})

test('uses frozen variable snapshots for #when operators', () => {
    const frozenCharacter = makeCharacter('Frozen')
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const parserContext = {
        db: frozenDatabase,
        chara: frozenCharacter,
        chatID: 1,
        chatVariables: { enabled: '1', score: 'Frozen score' },
        globalChatVariables: { toggle_enabled: '1' },
    }

    live.database.characters[0].chats[0].scriptstate = {
        $enabled: '0',
        $score: 'Live score',
    }
    live.database.globalChatVariables = { toggle_enabled: '0' }

    expect(risuChatParser(
        [
            '{{#when::var::enabled}}var{{/when}}',
            '{{#when::score::vis::Frozen score}}vis{{/when}}',
            '{{#when::toggle::enabled}}toggle{{/when}}',
            '{{#when::enabled::tis::1}}tis{{/when}}',
        ].join('|'),
        parserContext,
    )).toBe('var|vis|toggle|tis')
})

test('uses the supplied database when resolving the active member of a frozen group', () => {
    const member = makeCharacter('Frozen Member')
    const group = {
        type: 'group' as const,
        name: 'Frozen Group',
        chaId: 'group',
        chatPage: 0,
        chats: [{
            message: [{ role: 'char' as const, data: 'hello', saying: member.chaId }],
            note: '', name: '', localLore: [],
        }],
        characters: [member.chaId],
        customscript: [],
        globalLore: [],
    }
    const database = { characters: [group, member] } as Database

    expect(risuChatParser('{{char}}', {
        db: database,
        chara: group as any,
        selectedCharID: 0,
    })).toBe('Frozen Member')
})

test('uses frozen CBS values for the initial message and dynamic regex pattern', async () => {
    const frozenCharacter = makeCharacter('Frozen')
    const script: customscript = {
        comment: '',
        in: '^{{user}}$',
        out: '{{user}} matched',
        type: 'editdisplay',
        flag: 'g<cbs>',
        ableFlag: true,
    }
    frozenCharacter.customscript = [script]
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')

    const result = await processScriptFull(
        frozenCharacter,
        '{{user}}',
        'editdisplay',
        1,
        { chatRole: 'char' },
        {
            cache: 'bypass',
            captureContext: {
                presetRegex: [],
                moduleRegexScripts: [],
                moduleAssets: [],
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                parserContext: {
                    database: frozenDatabase,
                    character: frozenCharacter,
                    userName: 'Frozen user',
                    personaPrompt: 'Frozen persona',
                    modules: [],
                    moduleLorebooks: [],
                    selectedCharID: 0,
                    chatVariables: {},
                    globalChatVariables: {},
                    currentTime: 1,
                },
            },
        },
    )

    expect(result.data).toBe('Frozen user matched')
})

test('keeps real ParseMarkdown CBS output frozen when live state changes mid-capture', async () => {
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'user', data: 'previous' },
        { role: 'char', data: '{{user}}|{{persona}}|{{previoususerchat}}|{{moduleenabled::frozen-module}}' },
    ])
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 2,
        end: 2,
        renderContext: {
            character: null,
            characterName: frozenCharacter.name,
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: 'Frozen persona',
                modules: [{ id: 'frozen', name: 'Frozen', description: '', namespace: 'frozen-module' }] as any,
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: false,
                autoTranslateCachedOnly: false,
                translatorType: 'google',
                translateBeforeHTMLFormatting: false,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })

    live.database = makeDatabase(makeCharacter('Changed'), 'Changed')
    live.modules = [{ id: 'changed', name: 'Changed', namespace: 'changed-module' }]
    const capture = job.renderContext
    const output = await ParseMarkdown(
        job.messages[0].data,
        capture.parserContext.character as any,
        'back',
        job.start - 1,
        { chatRole: 'char' },
        {
            moduleAssets: capture.moduleAssets,
            hideAllImages: capture.settings.hideAllImages,
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext,
            } as any,
            projectedChatID: capture.firstParserMessageIndex,
        },
    )

    expect(output).toContain('Frozen user|Frozen persona|previous|1')
    expect(output).not.toContain('Changed')
})

test('keeps absolute chatindex while previouscharchat scans the projected frozen history', async () => {
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'char', data: 'earlier character turn' },
        { role: 'user', data: 'intervening user turn' },
        { role: 'char', data: '{{chatindex}}|{{previouscharchat}}' },
    ])
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 3,
        end: 3,
        renderContext: {
            character: null,
            characterName: frozenCharacter.name,
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: 'Frozen persona',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: false,
                autoTranslateCachedOnly: false,
                translatorType: 'google',
                translateBeforeHTMLFormatting: false,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })
    const capture = job.renderContext
    const output = await ParseMarkdown(
        job.messages[0].data,
        capture.parserContext.character as any,
        'back',
        job.start - 1,
        { chatRole: 'char' },
        {
            projectedChatID: capture.firstParserMessageIndex,
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext,
            },
        } as any,
    )

    expect(output).toContain('2|earlier character turn')

    const css = '.capture::after{content:"{{chatindex}}|{{previouscharchat}}"}'
    const styled = trimMarkdown(
        `<risu-style>${Buffer.from(css).toString('hex')}</risu-style>`,
        {
            parserContext: capture.parserContext as any,
            chatID: job.start - 1,
            projectedChatID: capture.firstParserMessageIndex,
            cbsConditions: { chatRole: 'char' },
        },
    )
    expect(styled).toContain('content:"2|earlier character turn"')
})

test('keeps the original final message and absolute id for a middle capture range', async () => {
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'char', data: 'opening character turn' },
        { role: 'user', data: 'first user turn' },
        { role: 'char', data: '{{lastmessage}}|{{lastmessageid}}' },
        { role: 'user', data: 'later user turn' },
        { role: 'char', data: 'original final turn' },
    ])
    const frozenDatabase = makeDatabase(frozenCharacter, 'Frozen')
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 3,
        end: 3,
        renderContext: {
            character: null,
            characterName: 'Frozen',
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [],
            moduleRegexScripts: [],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: '',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: false,
                autoTranslateCachedOnly: false,
                translatorType: 'google',
                translateBeforeHTMLFormatting: false,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })
    const capture = job.renderContext

    const output = await ParseMarkdown(
        job.messages[0].data,
        capture.parserContext.character as any,
        'back',
        job.start - 1,
        { chatRole: 'char' },
        {
            projectedChatID: capture.firstParserMessageIndex,
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext,
            },
        } as any,
    )

    expect(output).toContain('original final turn|4')
    expect(capture.parserContext.character.chats[0].message).toHaveLength(5)
})

test('uses frozen parser and regex inputs through real cached auto-translation', async () => {
    await clearLLMCache()
    const frozenCharacter = makeCharacter('Frozen', [
        { role: 'char', data: 'frozen previous character' },
        { role: 'user', data: 'intervening user' },
        { role: 'char', data: 'source text' },
    ])
    const frozenDatabase = {
        ...makeDatabase(frozenCharacter, 'Frozen'),
        translatorType: 'llm',
        translator: 'ko',
        translatorInputLanguage: 'en',
    } as Database
    const presetScript: customscript = {
        comment: '',
        in: '{{previouscharchat}}',
        out: 'frozen preset',
        type: 'edittrans',
        flag: 'g<cbs>',
        ableFlag: true,
    }
    const moduleScript: customscript = {
        comment: '',
        in: 'frozen preset',
        out: 'frozen module',
        type: 'edittrans',
    }
    const job = await createProductScreenshotJob({
        characterId: frozenCharacter.chaId,
        chatId: 'chat',
        messages: frozenCharacter.chats[0].message,
        start: 3,
        end: 3,
        renderContext: {
            character: frozenCharacter as any,
            characterName: frozenCharacter.name,
            characterImageSource: '',
            characterLargePortrait: false,
            userName: 'Frozen user',
            userImageSource: '',
            userLargePortrait: false,
            moduleAssets: [],
            presetRegex: [presetScript],
            moduleRegexScripts: [moduleScript],
            assetStyle: '',
            parserContext: {
                database: frozenDatabase,
                character: frozenCharacter,
                userName: 'Frozen user',
                personaPrompt: 'Frozen persona',
                modules: [],
                moduleLorebooks: [],
                selectedCharID: 0,
                chatVariables: {},
                globalChatVariables: {},
                currentTime: 1,
            },
            settings: {
                autoTranslate: true,
                autoTranslateCachedOnly: true,
                translatorType: 'llm',
                translateBeforeHTMLFormatting: true,
                legacyTranslation: false,
                showTranslationLoading: false,
                newImageHandlingBeta: false,
                assetWidth: -1,
                hideAllImages: false,
                iconSize: 100,
                zoomSize: 100,
                lineHeight: 1.25,
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                legacyMediaFindings: false,
                assetMaxDifference: 0.5,
            },
        },
    })
    await setLLMCache('source text', 'frozen previous character')

    const changedCharacter = makeCharacter('Changed', [
        { role: 'char', data: 'changed previous character' },
        { role: 'user', data: 'changed user' },
        { role: 'char', data: 'changed current' },
    ])
    live.database = {
        ...makeDatabase(changedCharacter, 'Changed'),
        translatorType: 'llm',
        translator: 'ja',
        translatorInputLanguage: 'en',
        presetRegex: [{ ...presetScript, in: 'changed previous character', out: 'changed preset' }],
    } as Database
    live.modules = [{ id: 'changed', name: 'Changed', namespace: 'changed-module' }]

    const capture = job.renderContext
    const result = await translateHTML(
        'source text',
        false,
        capture.character as any,
        job.start - 1,
        false,
        {
            scriptContext: {
                presetRegex: capture.presetRegex,
                moduleRegexScripts: capture.moduleRegexScripts,
                moduleAssets: capture.moduleAssets,
                dynamicAssets: capture.settings.dynamicAssets,
                dynamicAssetsEditDisplay: capture.settings.dynamicAssetsEditDisplay,
                parserContext: capture.parserContext as any,
            },
            projectedChatID: capture.firstParserMessageIndex,
            chara: capture.characterName,
            cbsConditions: { chatRole: 'char' },
        },
    )

    expect(result).toBe('frozen module')
    expect(result).not.toContain('Changed')
    await clearLLMCache()
})

test('does not reuse a live text-only translation cache entry during capture', async () => {
    const text = 'capture cache identity regression'
    const liveCharacter = makeCharacter('Live')
    live.database = {
        ...makeDatabase(liveCharacter, 'Live'),
        translatorType: 'bergamot',
        translator: 'live-target',
        translatorInputLanguage: 'en',
        aiModel: '',
        htmlTranslation: false,
        combineTranslation: false,
    } as Database
    expect(await translate(text, false)).toBe('en->live-target')

    const frozenCharacter = makeCharacter('Frozen')
    const frozenDatabase = {
        ...makeDatabase(frozenCharacter, 'Frozen'),
        translatorType: 'bergamot',
        translator: 'frozen-target',
        translatorInputLanguage: 'en',
        aiModel: '',
        htmlTranslation: false,
        combineTranslation: false,
    } as Database
    const result = await translateHTML(
        `<p>${text}</p>`,
        false,
        frozenCharacter,
        1,
        false,
        {
            scriptContext: {
                presetRegex: [],
                moduleRegexScripts: [],
                moduleAssets: [],
                dynamicAssets: false,
                dynamicAssetsEditDisplay: false,
                parserContext: {
                    database: frozenDatabase,
                    character: frozenCharacter,
                    userName: 'Frozen user',
                    personaPrompt: '',
                    modules: [],
                    moduleLorebooks: [],
                    selectedCharID: 0,
                    chatVariables: {},
                    globalChatVariables: {},
                    currentTime: 1,
                },
            },
            projectedChatID: 1,
            chara: frozenCharacter,
            cbsConditions: { chatRole: 'char' },
        },
    )

    expect(result).toContain('frozen-target')
    expect(result).not.toContain('live-target')
})

test('keeps chardisplayasset command and exclusions in the minimal frozen character', () => {
    const source = makeCharacter('Frozen')
    source.additionalAssets = [
        ['Visible display asset', 'visible.png', 'png'],
        ['Excluded display asset', 'excluded.png', 'png'],
    ]
    source.prebuiltAssetCommand = true
    source.prebuiltAssetExclude = ['excluded.png']
    const projected = snapshotChatScreenshotCharacter(source, source.chats[0])
    const database = { characters: [projected] } as Database

    live.database = makeDatabase(makeCharacter('Changed'), 'Changed')
    const output = risuChatParser('{{chardisplayasset}}', {
        db: database,
        chara: projected,
        selectedCharID: 0,
    })

    expect(output).toContain('Visible display asset')
    expect(output).not.toContain('Excluded display asset')
})
