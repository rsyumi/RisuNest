import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    database: {
        characters: [] as any[],
        statics: { imports: 0 },
    },
    commitDetachedCharacter: vi.fn(async (character: any, _reason: string) => {
        mocks.database.characters.push(character)
        return character.chaId
    }),
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
    readImage: vi.fn(),
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
    CharXWriter: class {},
}))
vi.mock('./process/modules', () => ({ exportModuleLegacy: vi.fn(), readModule: vi.fn() }))
vi.mock('@tauri-apps/plugin-fs', () => ({ readFile: vi.fn() }))
vi.mock('@tauri-apps/plugin-deep-link', () => ({ onOpenUrl: vi.fn() }))
vi.mock('./storage/accountStorage', () => ({ AccountStorage: class {} }))
vi.mock('./realmAccess', () => ({
    fetchRealmResource: vi.fn(),
    isRealmAccessDisabled: () => true,
}))
vi.mock('./media', () => ({ compressImage: vi.fn(), getImageType: vi.fn() }))

import { importCharacterProcess } from './characterCards'

describe('character card additions', () => {
    beforeEach(() => {
        mocks.database.characters = []
        mocks.database.statics.imports = 0
        mocks.nextId = 0
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
})
