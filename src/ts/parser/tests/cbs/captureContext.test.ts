import { writable } from 'svelte/store'
import { beforeEach, expect, test, vi } from 'vitest'
import type { Database, character, customscript } from '../../../storage/database.svelte'

const live = vi.hoisted(() => ({
    database: {} as Database,
    modules: [] as Array<{ id: string; name: string; namespace?: string }>,
}))

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

const { ParseMarkdown, risuChatParser } = await import('../../parser.svelte')
const { processScriptFull } = await import('../../../process/scripts')
const { createChatScreenshotJob } = await import('../../../chatScreenshotRange')

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

beforeEach(() => {
    live.database = makeDatabase(makeCharacter('Live'), 'Live')
    live.modules = [{ id: 'live', name: 'Live', namespace: 'live-module' }]
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
    const job = createChatScreenshotJob({
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
        capture.firstParserMessageIndex,
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
        },
    )

    expect(output).toContain('Frozen user|Frozen persona|previous|1')
    expect(output).not.toContain('Changed')
})
