import { readFileSync } from 'node:fs'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    compressImage: vi.fn(async (_data: Uint8Array) => new Uint8Array([0xde, 0xad, 0xbe, 0xef])),
    readImage: vi.fn(),
    saveAsset: vi.fn(async (_data: Uint8Array) => 'asset://roundtrip'),
}))

vi.mock('src/lang', () => ({
    language: {
        errors: { noData: 'no data' },
        successExport: 'exported',
    },
}))
vi.mock('../alert', () => ({
    alertClear: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertModuleSelect: vi.fn(),
    alertNormal: vi.fn(),
    alertStore: { set: vi.fn() },
    alertWait: vi.fn(),
}))
vi.mock('../storage/database.svelte', () => ({
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(),
    getDatabase: vi.fn(),
    setCurrentCharacter: vi.fn(),
    setDatabase: vi.fn(),
}))
vi.mock('../globalApi.svelte', () => ({
    AppendableBuffer: class {
        private chunks: Uint8Array[] = []

        append(data: Uint8Array) {
            this.chunks.push(data)
        }

        get buffer() {
            const length = this.chunks.reduce((total, chunk) => total + chunk.length, 0)
            const result = new Uint8Array(length)
            let offset = 0
            for (const chunk of this.chunks) {
                result.set(chunk, offset)
                offset += chunk.length
            }
            return result
        }
    },
    downloadFile: vi.fn(),
    forageStorage: {},
    LocalWriter: class {},
    readImage: mocks.readImage,
    saveAsset: mocks.saveAsset,
    VirtualWriter: class {},
}))
vi.mock('../util', () => ({
    checkPersonaBinded: vi.fn(),
    selectSingleFile: vi.fn(),
    sleep: vi.fn(),
}))
vi.mock('uuid', () => ({ v4: () => 'roundtrip-module-id' }))
vi.mock('./lorebook.svelte', () => ({ convertExternalLorebook: vi.fn() }))
vi.mock('../media', () => ({ compressImage: mocks.compressImage }))
vi.mock('../stores.svelte', () => ({
    DBState: { db: { modules: [] } },
    HideIconStore: { set: vi.fn() },
    moduleBackgroundEmbedding: { set: vi.fn() },
    ReloadGUIPointer: { set: vi.fn() },
}))
vi.mock('../interchangeability', () => ({
    convertCharacterToModule: vi.fn(),
    convertModuleToCharacter: vi.fn(),
}))
vi.mock('../characterCards', () => ({
    exportCharacterCard: vi.fn(),
    importCharacterProcess: vi.fn(),
}))

import { exportModuleLegacy, readModule, type RisuModule } from './modules'

describe('legacy module export', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        const rpackMap = readFileSync('src/ts/rpack/rpack_map.bin')
        vi.stubGlobal('fetch', vi.fn(async () => ({
            arrayBuffer: async () => rpackMap.buffer.slice(
                rpackMap.byteOffset,
                rpackMap.byteOffset + rpackMap.byteLength,
            ),
        })))
    })

    it('roundtrips ordinary asset bytes and preserves asset metadata without image compression', async () => {
        const originalBytes = new Uint8Array([0x00, 0xff, 0x13, 0x7a, 0x80, 0x42])
        mocks.readImage.mockResolvedValue(originalBytes)
        const module: RisuModule = {
            id: 'source-module-id',
            name: 'Byte exact module',
            description: 'Legacy asset roundtrip',
            assets: [['ordinary-asset', 'asset://source', 'bin']],
        }

        const exported = await exportModuleLegacy(module, { alertEnd: false, saveData: false })
        const imported = await readModule(Buffer.from(exported))

        expect(mocks.compressImage).not.toHaveBeenCalled()
        expect(mocks.saveAsset).toHaveBeenCalledTimes(1)
        expect(Buffer.from(mocks.saveAsset.mock.calls[0][0])).toEqual(Buffer.from(originalBytes))
        expect(imported.assets).toEqual([['ordinary-asset', 'asset://roundtrip', 'bin']])
    })
})
