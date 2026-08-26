import { beforeEach, describe, expect, it, vi } from 'vitest'
import goldenCardJson from './storage/tests/roadmap14/fixtures/charx/card-v3.json?raw'

const mocks = vi.hoisted(() => ({
    database: {
        characters: [] as any[],
        statics: { imports: 0 },
    },
    commitDetachedCharacter: vi.fn(async (character: any, _reason: string) => {
        mocks.database.characters.push(character)
        return character.chaId
    }),
    readImage: vi.fn(async (_key: string) => new Uint8Array([1, 2, 3, 4])),
    compressImage: vi.fn(async (_data: Uint8Array) => new Uint8Array([99])),
    charxWrites: [] as Array<{ key: string; data: Uint8Array }>,
    nextId: 0,
}))

vi.mock('uuid', () => ({ v4: () => `card-id-${++mocks.nextId}` }))
vi.mock('./characters', () => ({
    changeChar: vi.fn(),
    characterFormatUpdate: (value: unknown) => value,
    commitDetachedCharacter: mocks.commitDetachedCharacter,
}))
vi.mock('./storage/database.svelte', () => ({
    defaultSdDataFunc: () => ({}),
    getDatabase: () => mocks.database,
}))
vi.mock('./alert', () => ({
    alertCardExport: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertInput: vi.fn(),
    alertMd: vi.fn(),
    alertNormal: vi.fn(),
    alertStore: { set: vi.fn() },
    alertTOS: vi.fn(),
    alertWait: vi.fn(),
}))
vi.mock('./util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    decryptBuffer: vi.fn(),
    isKnownUri: vi.fn(),
    selectFileByDom: vi.fn(),
    sleep: vi.fn(),
}))
vi.mock('src/lang', () => ({ language: { errors: {}, importedCharacter: 'imported' } }))
vi.mock('./globalApi.svelte', () => ({
    AppendableBuffer: class {},
    BlankWriter: class {},
    checkCharOrder: vi.fn(),
    downloadFile: vi.fn(),
    forageStorage: {},
    loadAsset: vi.fn(),
    LocalWriter: class {},
    openURL: vi.fn(),
    readImage: mocks.readImage,
    saveAsset: vi.fn(),
    VirtualWriter: class {},
}))
vi.mock('src/ts/platform', () => ({ isTauri: false, isNodeServer: false }))
vi.mock('./stores.svelte', () => ({
    DBState: { db: mocks.database },
    SettingsMenuIndex: { set: vi.fn() },
    ShowRealmFrameStore: { set: vi.fn() },
    selectedCharID: { set: vi.fn() },
    settingsOpen: { set: vi.fn() },
}))
vi.mock('./parser/parser.svelte', () => ({ hasher: vi.fn() }))
vi.mock('./process/files/inlays', () => ({ reencodeImage: vi.fn() }))
vi.mock('./pngChunk', () => ({ PngChunk: {} }))
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
    readModule: vi.fn(),
}))
vi.mock('@tauri-apps/plugin-fs', () => ({ readFile: vi.fn() }))
vi.mock('@tauri-apps/plugin-deep-link', () => ({ getCurrent: vi.fn(), onOpenUrl: vi.fn() }))
vi.mock('./storage/accountStorage', () => ({ AccountStorage: class {} }))
vi.mock('./realmAccess', () => ({
    fetchRealmResource: vi.fn(),
    isRealmAccessDisabled: () => true,
}))
vi.mock('./media', () => ({
    compressImage: mocks.compressImage,
    getImageType: vi.fn(() => 'Unknown'),
}))

import {
    exportCharacterCard,
    importCharacterCardSpec,
    importCharacterProcess,
} from './characterCards'

describe('character card additions', () => {
    beforeEach(() => {
        mocks.database.characters = []
        mocks.database.statics.imports = 0
        mocks.nextId = 0
        mocks.charxWrites = []
        vi.clearAllMocks()
        mocks.commitDetachedCharacter.mockImplementation(async (character, _reason) => {
            mocks.database.characters.push(character)
            return character.chaId
        })
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
        expect(mocks.compressImage).not.toHaveBeenCalled()
    })
})
