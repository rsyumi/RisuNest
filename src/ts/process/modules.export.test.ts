import { readFileSync } from 'node:fs'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    readImage: vi.fn(),
    saveAsset: vi.fn(async (_data: Uint8Array) => ''),
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
vi.mock('../globalApi.svelte', async () => {
    const { AppendableBuffer } = await import('../appendableBuffer')
    return {
        AppendableBuffer,
        downloadFile: vi.fn(),
        forageStorage: {},
        LocalWriter: class {},
        readImage: mocks.readImage,
        saveAsset: mocks.saveAsset,
        VirtualWriter: class {},
    }
})
vi.mock('../util', () => ({
    checkPersonaBinded: vi.fn(),
    selectSingleFile: vi.fn(),
    sleep: vi.fn(),
}))
vi.mock('uuid', () => ({ v4: () => 'roundtrip-module-id' }))
vi.mock('./lorebook.svelte', () => ({ convertExternalLorebook: vi.fn() }))
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

    it('roundtrips ordinary asset bytes and preserves asset metadata exactly', async () => {
        const firstBytes = new Uint8Array([0x00, 0xff, 0x13, 0x7a, 0x80, 0x42])
        const secondBytes = new Uint8Array([0x91, 0x04, 0xcc, 0x2d, 0x7f])
        mocks.readImage.mockImplementation(async (source: string) => {
            if (source === 'asset://source-a') return firstBytes
            if (source === 'asset://source-b') return secondBytes
            throw new Error(`Unexpected asset source: ${source}`)
        })
        mocks.saveAsset
            .mockResolvedValueOnce('asset://roundtrip-a')
            .mockResolvedValueOnce('asset://roundtrip-b')
        const module: RisuModule = {
            id: 'source-module-id',
            name: 'Byte exact module',
            description: 'Legacy asset roundtrip',
            assets: [
                ['ordinary-asset-a', 'asset://source-a', 'bin', 'first-tail', { rank: 1 }],
                ['ordinary-asset-b', 'asset://source-b', 'dat', 'second-tail', { rank: 2 }],
            ] as unknown as [string, string, string][],
        }

        const exported = await exportModuleLegacy(module, { alertEnd: false, saveData: false })
        const imported = await readModule(Buffer.from(exported))

        expect(mocks.readImage.mock.calls.map(([source]) => source)).toEqual([
            'asset://source-a',
            'asset://source-b',
        ])
        expect(mocks.saveAsset).toHaveBeenCalledTimes(2)
        expect(mocks.saveAsset.mock.calls.map(([data]) => Array.from(data))).toEqual([
            Array.from(firstBytes),
            Array.from(secondBytes),
        ])
        expect(imported.assets).toEqual([
            ['ordinary-asset-a', 'asset://roundtrip-a', 'bin', 'first-tail', { rank: 1 }],
            ['ordinary-asset-b', 'asset://roundtrip-b', 'dat', 'second-tail', { rank: 2 }],
        ])
    })
})
