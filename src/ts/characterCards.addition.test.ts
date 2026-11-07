import { beforeEach, describe, expect, it, vi } from 'vitest'
import goldenCardJson from './storage/tests/roadmap14/fixtures/charx/card-v3.json?raw'

const mocks = vi.hoisted(() => ({
    database: {
        characters: [] as any[],
        modules: [] as any[],
        statics: { imports: 0 },
        blockedRealmCreators: [] as any[],
    },
    commitDetachedCharacter: vi.fn(async (character: any, _reason: string) => {
        mocks.database.characters.push(character)
        return character.chaId
    }),
    alertCardExport: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
    alertRisuServiceTOS: vi.fn(),
    readImage: vi.fn(async (_key: string) => new Uint8Array([1, 2, 3, 4])),
    saveAsset: vi.fn(),
    charxWrites: [] as Array<{ key: string; data: Uint8Array }>,
    pngWrites: [] as Array<{ key: string; data: Uint8Array }>,
    downloads: [] as Array<{ name: string; data: Uint8Array }>,
    platform: 'desktop' as 'desktop' | 'android' | 'ios',
    importAndroid: vi.fn(),
    importIOS: vi.fn(),
    nativeDesktopResult: { kind: 'cancelled' } as unknown,
    importDesktopNativeCharacterFromPicker: vi.fn(),
    importDesktopNativeCharacterPath: vi.fn(),
    importModuleContent: vi.fn(),
    desktopPickerPaths: [] as string[],
    openDesktopPicker: vi.fn(async () => mocks.desktopPickerPaths),
    exportNativeCharacterCharxFromPicker: vi.fn(),
    exportNativeCharacterCardFromPicker: vi.fn(),
    nextId: 0,
    destinationChosen: true,
}))

vi.mock('uuid', () => ({ v4: () => `card-id-${++mocks.nextId}` }))
vi.mock('./characters', () => ({
    changeChar: vi.fn(),
    characterFormatUpdate: (value: unknown) => value,
    commitDetachedCharacter: mocks.commitDetachedCharacter,
}))
vi.mock('./storage/database.svelte', () => ({
    defaultSdDataFunc: () => ({}),
    appVer: '0.0.0',
    getDatabase: () => mocks.database,
}))
vi.mock('./alert', () => ({
    alertCardExport: mocks.alertCardExport,
    alertConfirm: mocks.alertConfirm,
    alertError: mocks.alertError,
    alertInput: vi.fn(),
    alertMd: vi.fn(),
    alertNormal: mocks.alertNormal,
    alertStore: { set: vi.fn() },
    alertTOS: vi.fn(), alertRisuServiceTOS: mocks.alertRisuServiceTOS,
    alertWait: vi.fn(),
}))
vi.mock('./util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    decryptBuffer: vi.fn(),
    isKnownUri: vi.fn(),
    selectFileByDom: vi.fn(),
    sleep: vi.fn(),
}))
vi.mock('./globalApi.svelte', () => ({
    AppendableBuffer: class {},
    BlankWriter: class {
        async init() {}
        async write() {}
        async end() {}
    },
    checkCharOrder: vi.fn(),
    downloadFile: vi.fn(async (name: string, data: Uint8Array) => {
        mocks.downloads.push({ name, data: data.slice() })
    }),
    forageStorage: {},
    loadAsset: vi.fn(),
    LocalWriter: class {
        async init() { return mocks.destinationChosen }
    },
    openURL: vi.fn(),
    readImage: mocks.readImage,
    saveAsset: mocks.saveAsset,
    VirtualWriter: class {},
}))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule(() => ({
    runtime: 'tauri', os: mocks.platform === 'desktop' ? 'windows' : mocks.platform,
})))
vi.mock('./storage/androidContentPicker', () => ({ importAndroidContentFromPicker: mocks.importAndroid }))
vi.mock('./storage/iosContentPicker', () => ({ importIOSContentFromPicker: mocks.importIOS }))
vi.mock('./storage/nativeCharacterFileRoute', () => ({
    importDesktopNativeCharacterFromPicker: mocks.importDesktopNativeCharacterFromPicker,
    importDesktopNativeCharacterPath: mocks.importDesktopNativeCharacterPath,
}))
vi.mock('./storage/nativeCharacterCharxExportRoute', () => ({
    exportNativeCharacterCharxFromPicker: mocks.exportNativeCharacterCharxFromPicker,
}))
vi.mock('./storage/nativeCharacterCardExportRoute', () => ({
    exportNativeCharacterCardFromPicker: mocks.exportNativeCharacterCardFromPicker,
}))
vi.mock('@tauri-apps/plugin-dialog', () => ({
    open: mocks.openDesktopPicker,
}))
vi.mock('./stores.svelte', () => ({
    DBState: { db: mocks.database },
    SettingsMenuIndex: { set: vi.fn() },
    ShowRealmFrameStore: { set: vi.fn() },
    selectedCharID: { set: vi.fn() },
    settingsOpen: { set: vi.fn() },
}))
vi.mock('./parser/parser.svelte', async () => (await import('./parser/parser.testSupport')).parserStubModule({ hasher: vi.fn() }))
vi.mock('./process/files/inlays', () => ({ reencodeImage: vi.fn() }))
vi.mock('./pngChunk', () => ({
    PngChunk: {
        streamWriter: class {
            constructor(_image: Uint8Array, _writer: unknown) {}
            async init() {}
            async write(key: string, data: Uint8Array | string) {
                mocks.pngWrites.push({
                    key,
                    data: typeof data === 'string' ? new TextEncoder().encode(data) : data.slice(),
                })
            }
            async end() {}
        },
    },
}))
vi.mock('./process/processzip', () => ({
    CharXImporter: class {},
    CharXWriter: class {
        constructor(_writer: unknown) {}
        async init() {}
        async write(key: string, data: Uint8Array | string) {
            mocks.charxWrites.push({
                key,
                data: typeof data === 'string' ? new TextEncoder().encode(data) : data.slice(),
            })
        }
        async end() {}
    },
}))
vi.mock('./process/modules', () => ({
    exportModuleLegacy: vi.fn(async () => new Uint8Array([111, 0, 0, 0, 0, 0])),
    importPreparedNativeModuleContent: mocks.importModuleContent,
    readModule: vi.fn(),
}))
vi.mock('@tauri-apps/plugin-fs', () => ({ readFile: vi.fn() }))
vi.mock('@tauri-apps/plugin-deep-link', () => ({ getCurrent: vi.fn(), onOpenUrl: vi.fn() }))
vi.mock('./storage/accountStorage', () => ({ AccountStorage: class {} }))
vi.mock('./mobileBackgroundTask', async () => (await import('./mobileBackgroundTask.testSupport')).mobileBackgroundTaskModule())
vi.mock('./media', () => ({
    getImageType: vi.fn(() => 'Unknown'),
}))

import {
    characterURLImport,
    downloadRisuHub,
    exportCharacterCard,
    exportChar,
    getRealmInfo,
    getRisuHub,
    importCharacter,
    importCharacterCardSpec,
    importCharacterProcess,
    isNativeCharacterContentImportEnabled,
    mapPreparedNativeCharacterCard,
    showRealmInfoStore,
    type CharacterCardV2Risu,
} from './characterCards'
import {
    dismissNativeFileOperationOutcome,
    nativeFileOperationOutcome,
    NativeFileOperationBusyError,
    runSharedNativeFileOperation,
    type NativeFileOperationOutcome,
} from './storage/nativeFileJobManager'
import { get } from 'svelte/store'
import { language } from 'src/lang'

describe('character card additions', () => {
    beforeEach(() => {
        mocks.platform = 'desktop'
        mocks.database.characters = []
        mocks.database.statics.imports = 0
        mocks.database.blockedRealmCreators = []
        mocks.nextId = 0
        mocks.charxWrites = []
        mocks.pngWrites = []
        mocks.downloads = []
        mocks.nativeDesktopResult = { kind: 'cancelled' }
        mocks.desktopPickerPaths = []
        vi.clearAllMocks()
        mocks.alertConfirm.mockResolvedValue(true)
        mocks.alertCardExport.mockResolvedValue({ type: 'cancelled' })
        mocks.exportNativeCharacterCharxFromPicker.mockResolvedValue({ characterCount: 1 })
        mocks.exportNativeCharacterCardFromPicker.mockResolvedValue({ characterCount: 1 })
        mocks.commitDetachedCharacter.mockImplementation(async (character, _reason) => {
            mocks.database.characters.push(character)
            return character.chaId
        })
    })

    it.each(['ios', 'android'] as const)('routes the public character picker only to %s', async platform => {
        mocks.platform = platform
        mocks.importIOS.mockResolvedValue('ios-character')
        mocks.importAndroid.mockResolvedValue('android-character')
        await expect(importCharacter()).resolves.toBe(`${platform}-character`)
        expect(platform === 'ios' ? mocks.importIOS : mocks.importAndroid).toHaveBeenCalledWith('character')
        expect(platform === 'ios' ? mocks.importAndroid : mocks.importIOS).not.toHaveBeenCalled()
        expect(mocks.openDesktopPicker).not.toHaveBeenCalled()
    })

    it.each(['ios', 'android'] as const)('leaves a failed %s picker import to its dialog and reports a refused start', async platform => {
        mocks.platform = platform
        const picker = platform === 'ios' ? mocks.importIOS : mocks.importAndroid
        dismissNativeFileOperationOutcome()
        picker.mockImplementationOnce(() => runSharedNativeFileOperation('import', 'content-picker', async () => {
            throw new Error('synthetic invalid card')
        }, { presentation: 'dialog', format: 'content' }))
        await expect(importCharacter()).resolves.toBeNull()
        expect(get(nativeFileOperationOutcome)).toMatchObject({ kind: 'import', state: 'failed' })
        expect(mocks.alertError).not.toHaveBeenCalled()

        dismissNativeFileOperationOutcome()
        picker.mockRejectedValueOnce(new NativeFileOperationBusyError())
        await expect(importCharacter()).resolves.toBeNull()
        expect(mocks.alertError).toHaveBeenCalledExactlyOnceWith(language.risuNest.backup.fileBusy)
    })

    it('commits a complete detached legacy card before returning its stable index', async () => {
        const card = {
            name: 'Synthetic card',
            description: 'Description',
            first_mes: 'Hello',
        }

        const index = await importCharacterProcess({
            name: 'synthetic.json',
            data: new TextEncoder().encode(JSON.stringify(card)),
        })

        expect(mocks.commitDetachedCharacter).toHaveBeenCalledOnce()
        const [character, reason] = mocks.commitDetachedCharacter.mock.calls[0]
        expect(reason).toBe('import-character-card')
        expect(character.chaId).toBeTruthy()
        expect(character.chats).toHaveLength(1)
        expect(character.chats[0].id).toBeTruthy()
        expect(index).toBe(0)
    })

    it.each(['legacy', 'chara_card_v2', 'chara_card_v3'])('reuses parsed %s JSON through the same converter', async format => {
        const card = format === 'legacy'
            ? { char_name: '한글 😀', char_persona: 'Description', char_greeting: 'Hello' }
            : {
                spec: format,
                spec_version: format === 'chara_card_v2' ? '2.0' : '3.0',
                data: {
                    name: '한글 😀', description: 'Description', first_mes: 'Hello',
                    personality: '', scenario: '', mes_example: '', creator_notes: '',
                    system_prompt: '', post_history_instructions: '', alternate_greetings: [],
                    tags: [], creator: '', character_version: '', extensions: {}, assets: [],
                },
            }
        await importCharacterProcess({ name: 'synthetic.json', data: new TextEncoder().encode(JSON.stringify(card)) })
        const first = structuredClone(mocks.commitDetachedCharacter.mock.calls.at(-1)?.[0])
        mocks.database.characters = []
        mocks.nextId = 0
        await expect(importCharacterProcess({
            name: 'synthetic.json', data: Uint8Array.of(0xff), parsedJson: { value: card },
        })).resolves.toBe(0)
        const second = mocks.commitDetachedCharacter.mock.calls.at(-1)?.[0]
        expect(first).toBeDefined()
        expect(second).toEqual(first)
    })

    it('preserves browser malformed-byte replacement in JSON cards', async () => {
        const prefix = new TextEncoder().encode('{"name":"')
        const suffix = new TextEncoder().encode('","description":"Synthetic","first_mes":"Hello"}')
        await importCharacterProcess({
            name: 'synthetic.json', data: new Uint8Array([...prefix, 0xe2, 0x82, ...suffix]),
        })
        expect(mocks.commitDetachedCharacter.mock.calls[0][0].name).toBe('\uFFFD\uFFFD')
    })

    it('uses the native desktop character path route once and returns its imported character ID', async () => {
        mocks.desktopPickerPaths = ['C:\\chosen\\card.charx']
        mocks.nativeDesktopResult = {
            kind: 'imported',
            mode: 'native',
            value: 'native-character-id',
        }
        mocks.importDesktopNativeCharacterPath.mockResolvedValue(mocks.nativeDesktopResult)

        const imported = await importCharacter()

        expect(imported).toBe('native-character-id')
        expect(mocks.importDesktopNativeCharacterPath).toHaveBeenCalledOnce()
    })

    it('processes every selected desktop path through the native character route', async () => {
        mocks.desktopPickerPaths = [
            'C:\\chosen\\card.png',
            'C:\\chosen\\card.charx',
        ]
        mocks.importDesktopNativeCharacterPath
            .mockResolvedValueOnce({ kind: 'imported', mode: 'legacy', value: 'png-card' })
            .mockResolvedValueOnce({ kind: 'imported', mode: 'native', value: 'charx-card' })

        const imported = await importCharacter()

        expect(imported).toBe('charx-card')
        expect(mocks.openDesktopPicker).toHaveBeenCalledOnce()
        expect(mocks.openDesktopPicker).toHaveBeenCalledWith({
            multiple: true,
            directory: false,
        })
        expect(mocks.importDesktopNativeCharacterPath).toHaveBeenCalledTimes(2)
        expect(mocks.importDesktopNativeCharacterPath).toHaveBeenNthCalledWith(
            1,
            'C:\\chosen\\card.png',
            expect.any(Object),
        )
        expect(mocks.importDesktopNativeCharacterPath).toHaveBeenNthCalledWith(
            2,
            'C:\\chosen\\card.charx',
            expect.any(Object),
        )
    })

    it('continues a desktop batch after one selected file fails and reports the batch in one outcome', async () => {
        const error = new Error('synthetic invalid source')
        mocks.desktopPickerPaths = [
            'C:\\chosen\\first.charx',
            'C:\\chosen\\broken.charx',
            'C:\\chosen\\last.charx',
        ]
        mocks.importDesktopNativeCharacterPath
            .mockResolvedValueOnce({ kind: 'imported', mode: 'native', value: 'first-card' })
            .mockRejectedValueOnce(error)
            .mockResolvedValueOnce({ kind: 'imported', mode: 'native', value: 'last-card' })

        dismissNativeFileOperationOutcome()
        const outcomes: NativeFileOperationOutcome[] = []
        const unsubscribe = nativeFileOperationOutcome.subscribe(outcome => {
            if (outcome) outcomes.push(outcome)
        })
        try {
            await expect(importCharacter()).resolves.toBe('last-card')
        } finally {
            unsubscribe()
        }

        expect(mocks.importDesktopNativeCharacterPath).toHaveBeenCalledTimes(3)
        expect(mocks.importDesktopNativeCharacterPath).toHaveBeenNthCalledWith(
            3,
            'C:\\chosen\\last.charx',
            expect.any(Object),
        )
        const failure = { code: 'import-error', message: error.message, recoveryRequired: false }
        expect(outcomes).toEqual([expect.objectContaining({
            kind: 'import',
            format: 'content',
            state: 'failed',
            error: failure,
            batch: { total: 3, imported: 2, failures: [{ name: 'broken.charx', error: failure }], declined: [] },
        })])
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('imports several files a desktop file association opened in one batched outcome', async () => {
        const { consumeOpenedFiles } = await import('./openedFiles')
        await characterURLImport()
        mocks.importDesktopNativeCharacterPath
            .mockResolvedValueOnce({ kind: 'imported', mode: 'native', value: 'opened-card' })
            .mockResolvedValueOnce({ kind: 'imported', mode: 'native', value: 'single-card' })
        mocks.importModuleContent.mockResolvedValue({ kind: 'imported', value: 'opened-module' })
        dismissNativeFileOperationOutcome()
        const outcomes: NativeFileOperationOutcome[] = []
        const unsubscribe = nativeFileOperationOutcome.subscribe(outcome => {
            if (outcome) outcomes.push(outcome)
        })
        try {
            await consumeOpenedFiles(['C:\\opened\\card.charx', 'C:\\opened\\book.risum'])
            await consumeOpenedFiles(['C:\\opened\\single.charx'])
        } finally {
            unsubscribe()
        }

        expect(mocks.importModuleContent).toHaveBeenCalledExactlyOnceWith(
            { source: { type: 'desktopPath', path: 'C:\\opened\\book.risum' }, displayName: 'book.risum' },
            { signal: expect.any(AbortSignal), onStatus: expect.any(Function) },
        )
        expect(outcomes).toEqual([
            expect.objectContaining({ state: 'succeeded', batch: { total: 2, imported: 2, failures: [] } }),
            expect.objectContaining({ state: 'succeeded', source: { name: 'single.charx' } }),
        ])
        expect(outcomes[1]).not.toHaveProperty('batch')
        expect(mocks.alertError).not.toHaveBeenCalled()
    })

    it('keeps the JavaScript card fallback for mixed-case JSON filenames', async () => {
        const card = {
            name: 'Mixed case card',
            description: 'Description',
            first_mes: 'Hello',
        }

        const index = await importCharacterProcess({
            name: 'synthetic.JSON',
            data: new TextEncoder().encode(JSON.stringify(card)),
        })

        expect(mocks.commitDetachedCharacter).toHaveBeenCalledOnce()
        expect(index).toBe(0)
    })

    it('maps the bounded native card.json golden through the existing semantic mapper', async () => {
        const mapped = await importCharacterCardSpec(
            JSON.parse(goldenCardJson),
            undefined,
            'normal',
            {
                'assets/Portrait.JPEG': 'asset://portrait',
                'assets/config.JSON': 'asset://config',
            },
            null,
            true,
        )

        expect(mapped).toMatchObject({
            name: 'Roadmap 14 Golden Card',
            desc: 'Bounded native card metadata fixture',
            personality: 'Careful',
            scenario: 'Parser parity',
            firstMessage: 'Hello from CharX',
            image: 'asset://portrait',
            utilityBot: true,
            largePortrait: false,
            additionalAssets: [
                ['config', 'asset://config', 'JSON'],
                ['config duplicate', 'asset://config', 'JSON'],
            ],
            alternateGreetings: ['Second hello'],
            tags: ['roadmap14', 'golden'],
            nickname: 'Golden',
            source: ['synthetic'],
            creation_date: 1700000000,
            modification_date: 1700000001,
            extentions: {
                unknown_extension: {
                    ordered: ['first', 'second'],
                },
            },
        })
        expect(mocks.commitDetachedCharacter).not.toHaveBeenCalled()
    })

    it('maps prepared native metadata without reading or committing payloads', async () => {
        const card = JSON.parse(goldenCardJson)
        card.data.assets[0].uri = 'ccdefault:'
        const originalCard = structuredClone(card)
        const trigger = [{ comment: 'native trigger' }]
        const regex = [{ comment: 'native regex', in: 'a', out: 'b', type: 'editinput' }]
        const lorebook = [{
            key: 'native lore',
            secondkey: '',
            insertorder: 0,
            comment: 'native lore',
            content: 'native content',
            mode: 'normal',
            alwaysActive: false,
            selective: false,
        }]

        const mapped = await mapPreparedNativeCharacterCard({
            card,
            assets: [{ token: 'assets/config.JSON', logicalId: 'asset://config' }],
            portraitLogicalId: 'asset://portrait',
            module: { trigger, regex, lorebook } as any,
        })

        expect(mapped).toMatchObject({
            image: 'asset://portrait',
            additionalAssets: [
                ['config', 'asset://config', 'JSON'],
                ['config duplicate', 'asset://config', 'JSON'],
            ],
            triggerscript: trigger,
            customscript: regex,
            globalLore: lorebook,
        })
        expect(card).toEqual(originalCard)
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(mocks.commitDetachedCharacter).not.toHaveBeenCalled()
    })

    it('returns false when prepared native low-level access is declined', async () => {
        const card = JSON.parse(goldenCardJson)
        card.data.extensions.risuai.lowLevelAccess = true
        mocks.alertConfirm.mockResolvedValue(false)

        const mapped = await mapPreparedNativeCharacterCard({
            card,
            assets: [
                { token: 'assets/Portrait.JPEG', logicalId: 'asset://portrait' },
                { token: 'assets/config.JSON', logicalId: 'asset://config' },
            ],
        })

        expect(mapped).toBe(false)
        expect(mocks.alertConfirm).toHaveBeenCalledOnce()
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(mocks.commitDetachedCharacter).not.toHaveBeenCalled()
    })

    it('rejects prepared native v3 data URIs before payload work', async () => {
        const card = JSON.parse(goldenCardJson)
        card.data.assets[0].uri = 'data:image/png;base64,AA=='

        await expect(mapPreparedNativeCharacterCard({
            card,
            assets: [{ token: 'assets/config.JSON', logicalId: 'asset://config' }],
        })).rejects.toThrow(/data URI/i)
        expect(mocks.saveAsset).not.toHaveBeenCalled()
    })

    it('rejects non-main ccdefault assets without a prepared portrait', async () => {
        const card = JSON.parse(goldenCardJson)
        card.data.assets[0] = {
            type: 'emotion',
            uri: 'ccdefault:',
            name: 'fallback',
            ext: 'JPEG',
        }

        await expect(mapPreparedNativeCharacterCard({
            card,
            assets: [{ token: 'assets/config.JSON', logicalId: 'asset://config' }],
        })).rejects.toThrow(/portrait logical ID/i)
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(mocks.commitDetachedCharacter).not.toHaveBeenCalled()
    })

    it('rejects prepared native v2 inline base64 before payload work', async () => {
        const card:CharacterCardV2Risu = {
            spec: 'chara_card_v2',
            spec_version: '2.0',
            data: {
                name: 'Legacy native card',
                description: '',
                personality: '',
                scenario: '',
                first_mes: '',
                mes_example: '',
                creator_notes: '',
                system_prompt: '',
                post_history_instructions: '',
                alternate_greetings: [],
                tags: [],
                creator: '',
                character_version: '',
                extensions: { risuai: { emotions: [['happy', 'AA==']] } },
            },
        }

        await expect(mapPreparedNativeCharacterCard({
            card,
            assets: [],
        })).rejects.toThrow(/logical asset reference/i)
        expect(mocks.saveAsset).not.toHaveBeenCalled()
    })

    it('maps a prepared native v2 card through logical PNG chunk aliases', async () => {
        const card:CharacterCardV2Risu = {
            spec: 'chara_card_v2',
            spec_version: '2.0',
            data: {
                name: 'Legacy native card',
                description: 'Description',
                personality: '',
                scenario: '',
                first_mes: 'Hello',
                mes_example: '',
                creator_notes: '',
                system_prompt: '',
                post_history_instructions: '',
                alternate_greetings: [],
                tags: [],
                creator: '',
                character_version: '7',
                extensions: {
                    risuai: {
                        emotions: [['happy', '__asset:emotion']],
                        additionalAssets: [['file', '__asset:007', 'png']],
                    },
                },
            },
        }

        const mapped = await mapPreparedNativeCharacterCard({
            card,
            assets: [
                { token: 'emotion', logicalId: 'assets/emotion.png' },
                { token: '007', logicalId: 'assets/chunk.png' },
            ],
            portraitLogicalId: 'assets/portrait.png',
        })

        expect(mapped).toMatchObject({
            image: 'assets/portrait.png',
            emotionImages: [['happy', 'assets/emotion.png']],
            additionalAssets: [['file', 'assets/chunk.png', 'png']],
            characterVersion: '7',
        })
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(mocks.commitDetachedCharacter).not.toHaveBeenCalled()
    })

    it('maps an off-spec prepared PNG card without saving its portrait bytes', async () => {
        const mapped = await mapPreparedNativeCharacterCard({
            card: {
                avatar: 'none',
                chat: '',
                create_date: '',
                description: 'Description',
                first_mes: 'Hello',
                mes_example: '',
                name: 'Tavern',
                personality: '',
                scenario: '',
                talkativeness: '0.5',
            },
            assets: [],
            portraitLogicalId: 'assets/portrait.png',
        })

        expect(mapped).toMatchObject({
            name: 'Tavern',
            image: 'assets/portrait.png',
            firstMessage: 'Hello',
        })
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(mocks.commitDetachedCharacter).not.toHaveBeenCalled()
    })

    it('rejects duplicate prepared native asset tokens', async () => {
        const card = JSON.parse(goldenCardJson)

        await expect(mapPreparedNativeCharacterCard({
            card,
            assets: [
                { token: 'assets/Portrait.JPEG', logicalId: 'asset://portrait' },
                { token: 'assets/Portrait.JPEG', logicalId: 'asset://other' },
                { token: 'assets/config.JSON', logicalId: 'asset://config' },
            ],
        })).rejects.toThrow(/duplicate/i)
        expect(mocks.saveAsset).not.toHaveBeenCalled()
    })

    it('keeps ordinary asset bytes exact in the JavaScript CharX export fallback', async () => {
        const character = {
            type: 'character',
            name: 'Exact asset card',
            image: 'assets/avatar.bin',
            firstMessage: 'Hello',
            desc: '',
            chats: [],
            chatFolders: [],
            chatPage: 0,
            viewScreen: 'none',
            bias: [],
            emotionImages: [],
            globalLore: [],
            chaId: 'exact-asset-card',
            customscript: [],
            triggerscript: [],
            alternateGreetings: [],
            tags: [],
            additionalAssets: [],
            ccAssets: [{
                type: 'x-risu-asset',
                uri: 'assets/exact.bin',
                name: 'exact',
                ext: 'bin',
            }],
            extentions: {},
        } as any

        await exportCharacterCard(character, 'charx', {
            spec: 'v3',
            writer: {} as any,
        })

        const payload = mocks.charxWrites.find((entry) => entry.key.endsWith('/exact.bin'))
        expect(Array.from(payload?.data ?? [])).toEqual([1, 2, 3, 4])
    })

    it('holds a card export in the background only once a destination is chosen', async () => {
        const { beginMobileBackgroundTask } = await import('./mobileBackgroundTask')
        const character = { type: 'character', name: 'Held card', image: 'assets/avatar.png', emotionImages: [], additionalAssets: [], ccAssets: [], extentions: {} } as any
        for (const chosen of [false, true]) {
            mocks.destinationChosen = chosen
            await exportCharacterCard({ ...character }, 'png', { spec: 'v3' })
        }
        expect(beginMobileBackgroundTask).toHaveBeenCalledExactlyOnceWith('export', undefined, true)
    })

    it('routes the selected desktop CCv3 CharX through the native leased exporter', async () => {
        const character = {
            type: 'character',
            name: 'Native card',
            image: 'assets/avatar.png',
            firstMessage: 'Hello',
            desc: '',
            chats: [],
            chatFolders: [],
            chatPage: 0,
            viewScreen: 'none',
            bias: [],
            emotionImages: [],
            globalLore: [],
            chaId: 'native-card',
            customscript: [{ comment: 'regex' }],
            triggerscript: [{ comment: 'trigger' }],
            alternateGreetings: [],
            tags: [],
            additionalAssets: [],
            ccAssets: [],
            extentions: {},
        } as any
        mocks.database.characters = [character]
        mocks.alertCardExport.mockResolvedValue({
            type: '',
            type2: 'charx',
        } as any)

        await exportChar(0)

        expect(mocks.exportNativeCharacterCharxFromPicker).toHaveBeenCalledOnce()
        const input = mocks.exportNativeCharacterCharxFromPicker.mock.calls[0][0]
        expect(input.characterId).toBe('native-card')
        expect(input.suggestedName).toBe('Native card.charx')
        const leasedCharacter = {
            ...character,
            desc: 'Leased description',
            firstMessage: 'Leased greeting',
            alternateGreetings: ['Second leased greeting'],
            globalLore: [{ key: 'leased lore' }],
            extentions: { futureField: { kept: true } },
        }
        const projected = input.projectCharacter(leasedCharacter)
        expect(projected.card).toMatchObject({
            spec: 'chara_card_v3',
            data: {
                description: 'Leased description',
                first_mes: 'Leased greeting',
                alternate_greetings: ['Second leased greeting'],
                extensions: {
                    risuai: {},
                    futureField: { kept: true },
                },
            },
        })
        expect(projected.card.data.extensions.risuai).not.toHaveProperty('triggerscript')
        expect(projected.card.data.extensions.risuai).not.toHaveProperty('customScripts')
        expect(projected.module).toMatchObject({
            name: 'Native card Module',
            trigger: character.triggerscript,
            regex: character.customscript,
            lorebook: leasedCharacter.globalLore,
        })
        expect(mocks.readImage).not.toHaveBeenCalled()
        expect(mocks.charxWrites).toEqual([])
    })

    it('does not create a persistent fallback portrait before native CharX export', async () => {
        const fetchMock = vi.fn(async () => ({
            arrayBuffer: async () => new Uint8Array([9, 8, 7]).buffer,
        }))
        const originalFetch = globalThis.fetch
        globalThis.fetch = fetchMock as unknown as typeof fetch
        mocks.database.characters = [{
            type: 'character',
            name: 'Image-less card',
            image: '',
            chats: [],
            chaId: 'image-less-card',
            globalLore: [],
        }]
        mocks.alertCardExport.mockResolvedValue({ type: '', type2: 'charx' } as any)

        try {
            await exportChar(0)
        }
        finally {
            globalThis.fetch = originalFetch
        }

        expect(mocks.exportNativeCharacterCharxFromPicker).toHaveBeenCalledOnce()
        expect(fetchMock).not.toHaveBeenCalled()
        expect(mocks.saveAsset).not.toHaveBeenCalled()
    })

    it('routes appended JPEG through native CharX with the explicit container discriminator', async () => {
        mocks.database.characters = [{
            type: 'character',
            name: 'JPEG card',
            image: 'assets/avatar.png',
            chats: [],
            chaId: 'jpeg-card',
            globalLore: [],
            customscript: [],
            triggerscript: [],
        }]
        mocks.alertCardExport.mockResolvedValue({ type: '', type2: 'charxJpeg' } as any)

        await exportChar(0)

        expect(mocks.exportNativeCharacterCharxFromPicker).toHaveBeenCalledWith(expect.objectContaining({
            characterId: 'jpeg-card',
            suggestedName: 'JPEG card.jpeg',
            container: 'appended-charx-jpeg',
        }))
        expect(mocks.readImage).not.toHaveBeenCalled()
        expect(mocks.charxWrites).toEqual([])
    })

    it('routes selected CCv3 JSON through native export without renderer payload bytes', async () => {
        const character = {
            type: 'character',
            name: 'Native JSON card',
            image: '',
            firstMessage: 'Hello',
            desc: 'Description',
            chats: [],
            chatFolders: [],
            chatPage: 0,
            viewScreen: 'none',
            bias: [],
            emotionImages: [],
            globalLore: [],
            chaId: 'native-json-card',
            customscript: [{ comment: 'regex' }],
            triggerscript: [{ comment: 'trigger' }],
            alternateGreetings: [],
            tags: [],
            additionalAssets: [],
            ccAssets: [],
            extentions: {},
        } as any
        mocks.database.characters = [character]
        mocks.alertCardExport.mockResolvedValue({ type: '', type2: 'json' } as any)

        await exportChar(0)

        expect(mocks.exportNativeCharacterCardFromPicker).toHaveBeenCalledOnce()
        const input = mocks.exportNativeCharacterCardFromPicker.mock.calls[0][0]
        expect(input).toMatchObject({
            characterId: 'native-json-card',
            suggestedName: 'Native JSON card.json',
            format: 'json-card',
        })
        const projected = input.projectCharacter(character)
        expect(projected.data.extensions.risuai.triggerscript).toEqual(character.triggerscript)
        expect(projected.data.extensions.risuai.customScripts).toEqual(character.customscript)
        expect(JSON.stringify(projected)).not.toMatch(/data:[^,]*;base64/)
        expect(mocks.readImage).not.toHaveBeenCalled()
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(mocks.downloads).toEqual([])
    })

    it('routes selected CCv3 PNG through the native card exporter', async () => {
        const character = {
            type: 'character',
            name: 'Native PNG card',
            image: 'assets/avatar.png',
            chats: [],
            chaId: 'native-png-card',
            globalLore: [],
            customscript: [],
            triggerscript: [],
            alternateGreetings: [],
            tags: [],
            additionalAssets: [],
            ccAssets: [],
            extentions: {},
        } as any
        mocks.database.characters = [character]
        mocks.alertCardExport.mockResolvedValue({ type: '', type2: 'png' } as any)

        await exportChar(0)

        expect(mocks.exportNativeCharacterCardFromPicker).toHaveBeenCalledOnce()
        expect(mocks.exportNativeCharacterCardFromPicker).toHaveBeenCalledWith(expect.objectContaining({
            characterId: 'native-png-card',
            suggestedName: 'Native PNG card.png',
            format: 'png-card',
        }))
        expect(mocks.readImage).not.toHaveBeenCalled()
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(mocks.pngWrites).toEqual([])
        expect(mocks.downloads).toEqual([])
        // The export dialog reports the saved file.
        expect(mocks.alertNormal).not.toHaveBeenCalled()
    })

    it('shows the established export error alert when native CharX export fails', async () => {
        const error = new Error('destination is full')
        mocks.database.characters = [{
            type: 'character',
            name: 'Native card',
            image: 'assets/avatar.png',
            chats: [],
            chaId: 'native-card',
            globalLore: [],
        }]
        mocks.alertCardExport.mockResolvedValue({ type: '', type2: 'charx' } as any)
        mocks.exportNativeCharacterCharxFromPicker.mockRejectedValue(error)

        await expect(exportChar(0)).resolves.toBe('')

        expect(mocks.alertError).toHaveBeenCalledWith(error)

        mocks.exportNativeCharacterCharxFromPicker.mockRejectedValue(
            Object.assign(new Error('Another library operation is running'), { code: 'library-operation-busy' }))
        await expect(exportChar(0)).resolves.toBe('')
        expect(mocks.alertError).toHaveBeenLastCalledWith(language.risuNest.importDialog.reasonBusy)
    })

    it('keeps ordinary asset bytes exact in the JavaScript JSON export fallback', async () => {
        const character = {
            type: 'character',
            name: 'JSON exact asset card',
            image: 'assets/avatar.bin',
            firstMessage: 'Hello',
            desc: '',
            chats: [],
            chatFolders: [],
            chatPage: 0,
            viewScreen: 'none',
            bias: [],
            emotionImages: [],
            globalLore: [],
            chaId: 'json-exact-asset-card',
            customscript: [],
            triggerscript: [],
            alternateGreetings: [],
            tags: [],
            additionalAssets: [],
            ccAssets: [{
                type: 'x-risu-asset',
                uri: 'assets/exact.bin',
                name: 'exact',
                ext: 'bin',
            }],
            extentions: {},
        } as any

        await exportCharacterCard(character, 'json', { spec: 'v3' })

        const exported = JSON.parse(new TextDecoder().decode(mocks.downloads[0].data))
        const encoded = exported.data.assets[0].uri.split(',')[1]
        expect(Array.from(Buffer.from(encoded, 'base64'))).toEqual([1, 2, 3, 4])
    })

    it('keeps ordinary asset bytes exact in the JavaScript PNG export fallback', async () => {
        const character = {
            type: 'character',
            name: 'PNG exact asset card',
            image: 'assets/avatar.bin',
            firstMessage: 'Hello',
            desc: '',
            chats: [],
            chatFolders: [],
            chatPage: 0,
            viewScreen: 'none',
            bias: [],
            emotionImages: [],
            globalLore: [],
            chaId: 'png-exact-asset-card',
            customscript: [],
            triggerscript: [],
            alternateGreetings: [],
            tags: [],
            additionalAssets: [],
            ccAssets: [{
                type: 'x-risu-asset',
                uri: 'assets/exact.bin',
                name: 'exact',
                ext: 'bin',
            }],
            extentions: {},
        } as any

        await exportCharacterCard(character, 'png', {
            spec: 'v3',
            writer: {} as any,
        })

        const payload = mocks.pngWrites.find((entry) => entry.key === 'chara-ext-asset_:1')
        expect(Array.from(Buffer.from(new TextDecoder().decode(payload?.data), 'base64')))
            .toEqual([1, 2, 3, 4])
    })

    const plainCharacter = (overrides: Record<string, unknown> = {}) => ({
        type: 'character', name: 'Plain card', image: 'assets/avatar.png', firstMessage: 'Hello', desc: '',
        chats: [], chatFolders: [], chatPage: 0, viewScreen: 'none', bias: [], emotionImages: [], globalLore: [],
        chaId: 'plain-card', customscript: [], triggerscript: [], alternateGreetings: [], tags: [],
        additionalAssets: [], ccAssets: [], extentions: {}, ...overrides,
    }) as any
    const exportedJson = (index: number) => JSON.parse(new TextDecoder().decode(mocks.downloads[index].data))
    async function withFetch<T>(handler: (url: string) => unknown, run: () => Promise<T>) {
        const fetchMock = vi.fn(async (url: string) => handler(String(url)))
        const originalFetch = globalThis.fetch
        globalThis.fetch = fetchMock as unknown as typeof fetch
        try {
            await run()
        } finally {
            globalThis.fetch = originalFetch
        }
        return fetchMock.mock.calls.map(([url]) => String(url))
    }

    it('exports the default portrait when the character image cannot be read', async () => {
        const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
        const fallback = { ok: true, status: 200, statusText: 'OK', arrayBuffer: async () => new Uint8Array([9, 8, 7]).buffer }
        try {
            const requests = await withFetch(() => fallback, async () => {
                mocks.readImage.mockRejectedValueOnce(new Error('synthetic unreadable image'))
                await exportCharacterCard(plainCharacter(), 'json', { spec: 'v3' })
                mocks.readImage.mockResolvedValueOnce(new Uint8Array())
                await exportCharacterCard(plainCharacter(), 'json', { spec: 'v3' })
            })
            expect(requests).toEqual(['/none.webp', '/none.webp'])
        } finally {
            warn.mockRestore()
        }
        for (const index of [0, 1]) {
            const portrait = exportedJson(index).data.assets.find((asset: any) => asset.type === 'icon' && asset.name === 'main')
            expect(Array.from(Buffer.from(portrait.uri.split(',')[1], 'base64'))).toEqual([9, 8, 7])
        }

        const missing = { ok: false, status: 404, statusText: 'Not Found', arrayBuffer: async () => new ArrayBuffer(0) }
        mocks.readImage.mockResolvedValueOnce(new Uint8Array())
        let failure: unknown
        await withFetch(() => missing, async () => {
            failure = await exportCharacterCard(plainCharacter(), 'json', { spec: 'v3' }).catch((error: unknown) => error)
        })
        expect(String(failure)).toContain('404')
        expect(mocks.downloads).toHaveLength(2)
    })

    it('carries the onboarding through a V3 card export and import', async () => {
        const onboarding = {
            enabled: true, mode: 'form', title: 'Start', description: 'Pick a start', html: '', floatingButton: true,
            questions: [{
                id: 'q1', title: 'Route', description: '', type: 'multiple', varName: 'route', required: true, placeholder: '',
                options: [{ label: 'Forest', value: 'forest' }],
            }],
        }
        await exportCharacterCard(plainCharacter({ onboarding }), 'json', { spec: 'v3' })
        const card = exportedJson(0)
        expect(card.data.extensions.risuai.onboarding).toEqual(onboarding)
        card.data.assets = []
        const imported = await importCharacterCardSpec(structuredClone(card), undefined, 'normal', {}, null, true)
        expect(imported && imported.onboarding).toEqual(onboarding)

        card.data.extensions.risuai.onboarding = {
            enabled: 'yes', mode: 'canvas', title: 3,
            questions: [{ type: 'slider', varName: 'mood', options: [{ label: 'Calm' }, null] }, null],
        }
        const coerced = await importCharacterCardSpec(structuredClone(card), undefined, 'normal', {}, null, true)
        expect(coerced && coerced.onboarding).toEqual({
            enabled: false, mode: 'form', title: '', description: '', html: '', floatingButton: false,
            questions: [{
                id: expect.any(String), title: '', description: '', type: 'single', varName: 'mood', required: false, placeholder: '',
                options: [{ label: 'Calm', value: '' }],
            }],
        })
        delete card.data.extensions.risuai.onboarding
        const without = await importCharacterCardSpec(structuredClone(card), undefined, 'normal', {}, null, true)
        expect(without && without.onboarding).toBeUndefined()
    })

    it('imports a card lorebook entry without content as empty text', async () => {
        await exportCharacterCard(plainCharacter(), 'json', { spec: 'v3' })
        const card = exportedJson(0)
        card.data.assets = []
        card.data.character_book = {
            extensions: {},
            entries: [{ keys: ['synthetic'], content: null, extensions: {}, enabled: true, insertion_order: 100 }],
        }
        const imported = await importCharacterCardSpec(structuredClone(card), undefined, 'normal', {}, null, true)
        expect(imported && imported.globalLore[0]).toMatchObject({ key: 'synthetic', content: '' })
    })

    it('stops a Realm download or preview from a blocked creator before anything is downloaded', async () => {
        mocks.database.blockedRealmCreators = [{ id: 'blocked-creator', name: 'Blocked' }]
        const info = (creator: string) => ({ status: 200, json: async () => ({ id: 'synthetic-card', creator }), text: async () => '' })
        showRealmInfoStore.set(null)

        const requests = await withFetch(() => info('blocked-creator'), async () => {
            await downloadRisuHub('synthetic-card')
            await downloadRisuHub('synthetic-card', { creator: 'blocked-creator' })
            await getRealmInfo('synthetic-card')
        })
        expect(requests).toHaveLength(2)
        expect(requests.every(url => url.endsWith('/hub/info/synthetic-card'))).toBe(true)
        expect(mocks.alertNormal.mock.calls).toEqual(Array(3).fill([language.realmCreatorBlocked]))
        expect(mocks.alertRisuServiceTOS).not.toHaveBeenCalled()
        expect(get(showRealmInfoStore)).toBeNull()

        await withFetch(() => info('allowed-creator'), async () => {
            await downloadRisuHub('synthetic-card')
            await getRealmInfo('synthetic-card')
        })
        expect(mocks.alertRisuServiceTOS).toHaveBeenCalledOnce()
        expect(get(showRealmInfoStore)).toMatchObject({ creator: 'allowed-creator' })
        expect(mocks.alertNormal).toHaveBeenCalledTimes(3)
    })

    it('hides cards from blocked creators in Realm listings', async () => {
        mocks.database.blockedRealmCreators = [{ id: 'blocked-creator', name: 'Blocked' }]
        const cards = [{ id: 'kept', creator: 'allowed-creator' }, { id: 'hidden', creator: 'blocked-creator' }, { id: 'anonymous' }]
        const listed: unknown[] = []
        for (const body of [cards, { cards, additionalHTML: '' }]) {
            await withFetch(() => ({ status: 200, json: async () => structuredClone(body) }), async () => {
                listed.push((await getRisuHub({ search: '', page: 0, nsfw: false, sort: '' })).map(card => card.id))
            })
        }
        expect(listed).toEqual([['kept', 'anonymous'], ['kept', 'anonymous']])
    })

    it('reports a failed creator lookup and does not download the Realm card', async () => {
        const requests = await withFetch(() => ({ status: 503, text: async () => 'synthetic lookup failure' }), async () => {
            await downloadRisuHub('synthetic-card')
        })
        expect(requests).toEqual([expect.stringMatching(/\/hub\/info\/synthetic-card$/)])
        expect(String(mocks.alertError.mock.calls[0]?.[0])).toContain('synthetic lookup failure')
        expect(mocks.alertRisuServiceTOS).not.toHaveBeenCalled()
    })

    it('drops MCP data from a module imported by URL', async () => {
        mocks.database.modules = []
        const module = { type: 'risuModule', id: 'source', name: 'Synthetic', description: '', mcp: { url: 'internal:dice' } }
        location.hash = '#import_module=' + encodeURIComponent(Buffer.from(JSON.stringify(module)).toString('base64'))
        try {
            await characterURLImport()
        } finally {
            location.hash = ''
        }
        expect(mocks.database.modules).toHaveLength(1)
        expect(mocks.database.modules[0]).toMatchObject({ name: 'Synthetic', id: 'card-id-1' })
        expect(mocks.database.modules[0]).not.toHaveProperty('mcp')
        expect(mocks.alertError).not.toHaveBeenCalled()
    })
})
