import { readFileSync } from 'node:fs'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    platform: 'web' as 'web' | 'desktop' | 'ios' | 'android',
    importIOS: vi.fn(), importAndroid: vi.fn(),
    open: vi.fn(),
    readFile: vi.fn(),
    selectMultipleFile: vi.fn(),
    readImage: vi.fn(),
    saveAsset: vi.fn(async (_data: Uint8Array) => ''),
    exportRisum: vi.fn(),
}))

vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => ({
    runtime: mocks.platform === 'web' ? 'web' : 'tauri',
    os: mocks.platform === 'ios' || mocks.platform === 'android' ? mocks.platform : 'windows',
})))
vi.mock('../storage/iosContentPicker', () => ({ importIOSContentFromPicker: mocks.importIOS }))
vi.mock('../storage/androidContentPicker', () => ({ importAndroidContentFromPicker: mocks.importAndroid }))
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: mocks.open }))
vi.mock('@tauri-apps/plugin-fs', () => ({ readFile: mocks.readFile }))
vi.mock('../storage/nativeModuleRisumExportRoute', () => ({ exportNativeModuleRisumFromPicker: mocks.exportRisum }))
vi.mock('../mobileBackgroundTask', async () => (await import('../mobileBackgroundTask.testSupport')).mobileBackgroundTaskModule())

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
    selectMultipleFile: mocks.selectMultipleFile,
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

import { get } from 'svelte/store'
import { language } from 'src/lang'
import { exportModuleLegacy, importModule, importModuleData, readModule, type RisuModule } from './modules'
import { DBState } from '../stores.svelte'
import {
    dismissNativeFileOperationOutcome,
    nativeFileOperationOutcome,
    NativeFileOperationBusyError,
    runSharedNativeFileOperation,
} from '../storage/nativeFileJobManager'
import { alertConfirm, alertError, alertNormal } from '../alert'
import { StdioModuleImportError } from './mcp/moduleImport'
import { convertExternalLorebook } from './lorebook.svelte'

describe('legacy module export', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.platform = 'web'
        dismissNativeFileOperationOutcome()
        DBState.db.modules = []
        const rpackMap = readFileSync('src/ts/rpack/rpack_map.bin')
        vi.stubGlobal('fetch', vi.fn(async () => ({
            arrayBuffer: async () => rpackMap.buffer.slice(
                rpackMap.byteOffset,
                rpackMap.byteOffset + rpackMap.byteLength,
            ),
        })))
    })

    it.each([
        { type: 'risuModule', id: 'source', name: '한글 😀', lorebook: [] },
        { type: 'risu', data: [{ key: 'synthetic', content: '한글 😀' }] },
        { entries: { synthetic: { content: '한글 😀' } } },
        { type: 'regex', data: [{ in: 'synthetic', out: '한글 😀' }] },
    ])('reuses parsed JSON without decoding while retaining the converter: %j', async value => {
        vi.mocked(convertExternalLorebook).mockReturnValue([{ key: 'converted', content: '한글 😀' }] as any)
        await importModuleData({ name: 'synthetic.json', data: new TextEncoder().encode(JSON.stringify(value)) })
        const first = structuredClone(DBState.db.modules[0])
        DBState.db.modules = []
        await importModuleData({ name: 'synthetic.json', data: Uint8Array.of(0xff), parsedJson: { value } })
        expect(first).toBeDefined()
        expect(DBState.db.modules[0]).toEqual(first)
    })

    it('preserves browser malformed-byte replacement in JSON modules', async () => {
        const prefix = new TextEncoder().encode('{"type":"risuModule","id":"synthetic","name":"')
        await importModuleData({
            name: 'synthetic.json', data: new Uint8Array([...prefix, 0xe2, 0x82, 0x22, 0x7d]),
        })
        expect(DBState.db.modules[0].name).toBe('\uFFFD\uFFFD')
    })

    it.each([false, true, undefined])('rejects JSON stdio modules regardless of lowLevelAccess=%s', async lowLevelAccess => {
        await importModuleData({
            name: 'synthetic.json',
            data: Buffer.from(JSON.stringify({
                type: 'risuModule', id: 'source', name: 'Synthetic', lowLevelAccess,
                mcp: { url: 'stdio:{"command":"node","args":["synthetic.js"]}' },
            })),
        })
        expect(DBState.db.modules).toEqual([])
        expect(alertConfirm).not.toHaveBeenCalled()
        expect(alertError).toHaveBeenCalledWith(expect.any(StdioModuleImportError))
        expect(alertNormal).not.toHaveBeenCalled()
    })

    it('reports a native RISUM export refused while the library is busy, and leaves success to the export dialog', async () => {
        mocks.platform = 'desktop'
        const module = { id: 'source', name: 'Synthetic', description: '' }
        mocks.exportRisum.mockRejectedValueOnce(
            Object.assign(new Error('Another library operation is running'), { code: 'library-operation-busy' }))
        await expect(exportModuleLegacy(module)).resolves.toEqual(new Uint8Array(0))
        expect(alertError).toHaveBeenCalledExactlyOnceWith(language.risuNest.importDialog.reasonBusy)
        mocks.exportRisum.mockResolvedValueOnce({ revision: 1 })
        await exportModuleLegacy(module)
        expect(alertNormal).not.toHaveBeenCalled()
    })

    it.each([false, true])('rejects legacy RISUM stdio before saving assets with lowLevelAccess=%s', async lowLevelAccess => {
        mocks.readImage.mockResolvedValue(new Uint8Array([1, 2, 3]))
        const exported = await exportModuleLegacy({
            id: 'source', name: 'Synthetic', description: '', lowLevelAccess,
            mcp: { url: 'stdio:{"command":"node","args":["synthetic.js"]}' },
            assets: [['synthetic', 'asset://synthetic', 'bin']],
        }, { alertEnd: false, saveData: false })

        await expect(readModule(Buffer.from(exported))).rejects.toBeInstanceOf(StdioModuleImportError)
        await importModuleData({ name: 'synthetic.risum', data: exported })
        expect(mocks.saveAsset).not.toHaveBeenCalled()
        expect(DBState.db.modules).toEqual([])
        expect(alertError).toHaveBeenCalledWith(expect.any(StdioModuleImportError))
    })

    it.each(['https://synthetic.invalid/mcp', 'internal:dice', 'plugin:synthetic'])('drops %s from JSON and RISUM imports', async url => {
        const module = { id: 'source', name: 'Synthetic', description: '', mcp: { url } }
        await importModuleData({
            name: 'synthetic.json', data: Buffer.from(JSON.stringify({ ...module, type: 'risuModule' })),
        })
        const exported = await exportModuleLegacy(module, { alertEnd: false, saveData: false })
        await importModuleData({ name: 'synthetic.risum', data: exported })
        expect(DBState.db.modules.map(module => module.name)).toEqual(['Synthetic', 'Synthetic'])
        expect(DBState.db.modules.map(module => Object.hasOwn(module, 'mcp'))).toEqual([false, false])
        expect(alertError).not.toHaveBeenCalled()
    })

    it.each(['ios', 'android'] as const)('routes the public module picker only to %s', async platform => {
        mocks.platform = platform
        mocks.importIOS.mockResolvedValue('ios-module')
        mocks.importAndroid.mockResolvedValue('android-module')
        await importModule()
        expect(mocks.importIOS).toHaveBeenCalledTimes(platform === 'ios' ? 1 : 0)
        expect(mocks.importAndroid).toHaveBeenCalledTimes(platform === 'android' ? 1 : 0)
        if (platform === 'ios') expect(mocks.importIOS).toHaveBeenCalledWith('module')
        if (platform === 'android') expect(mocks.importAndroid).toHaveBeenCalledWith('module')
        expect(mocks.open).not.toHaveBeenCalled()
    })

    const moduleJson = (name: string) => new TextEncoder().encode(JSON.stringify({ type: 'risuModule', id: name, name }))

    it('imports every module picked on desktop in one dialog', async () => {
        mocks.platform = 'desktop'
        mocks.open.mockResolvedValue(['C:/synthetic/first.json', 'C:/synthetic/second.lorebook'])
        mocks.readFile.mockImplementation(async (path: string) => moduleJson(path.includes('first') ? 'First' : 'Second'))

        await expect(importModule()).resolves.toBe('module')

        expect(mocks.open).toHaveBeenCalledWith(expect.objectContaining({ multiple: true }))
        expect(DBState.db.modules.map(module => module.name)).toEqual(['First', 'Second'])
        expect(get(nativeFileOperationOutcome)).toMatchObject({
            kind: 'import', state: 'succeeded', batch: { total: 2, imported: 2, failures: [], declined: [] },
        })
    })

    it('imports every module picked on the web', async () => {
        mocks.selectMultipleFile.mockResolvedValue([
            { name: 'first.json', data: moduleJson('First') },
            { name: 'second.json', data: moduleJson('Second') },
        ])
        await expect(importModule()).resolves.toBe('module')
        expect(mocks.selectMultipleFile).toHaveBeenCalledWith(['json', 'lorebook', 'risum', 'charx'])
        expect(DBState.db.modules.map(module => module.name)).toEqual(['First', 'Second'])
    })

    it.each(['ios', 'android'] as const)('leaves a failed %s picker import to its dialog and reports a refused start', async platform => {
        mocks.platform = platform
        const picker = platform === 'ios' ? mocks.importIOS : mocks.importAndroid
        picker.mockImplementationOnce(() => runSharedNativeFileOperation('import', 'content-picker', async () => {
            throw new Error('synthetic invalid module')
        }, { presentation: 'dialog', format: 'content' }))
        await expect(importModule()).resolves.toBeNull()
        expect(get(nativeFileOperationOutcome)).toMatchObject({ kind: 'import', state: 'failed' })
        expect(alertError).not.toHaveBeenCalled()

        dismissNativeFileOperationOutcome()
        picker.mockRejectedValueOnce(new NativeFileOperationBusyError())
        await expect(importModule()).resolves.toBeNull()
        expect(alertError).toHaveBeenCalledExactlyOnceWith(language.risuNest.backup.fileBusy)
    })

    it('lists the desktop modules that could not be read or were refused in the dialog and reports a refused start', async () => {
        mocks.platform = 'desktop'
        mocks.open.mockResolvedValue(['C:/synthetic/first.json', 'C:/synthetic/missing.json', 'C:/synthetic/unnamed.json'])
        mocks.readFile.mockImplementation(async (path: string) => {
            if (path.includes('missing')) throw Object.assign(new Error('synthetic read failure'), { code: 'io-error' })
            return moduleJson(path.includes('unnamed') ? '' : 'First')
        })
        await expect(importModule()).resolves.toBe('module')
        expect(DBState.db.modules.map(module => module.name)).toEqual(['First'])
        expect(get(nativeFileOperationOutcome)).toMatchObject({
            state: 'failed',
            batch: {
                total: 3,
                imported: 1,
                failures: [{ name: 'missing.json', error: { code: 'io-error' } }],
                declined: ['unnamed.json'],
            },
        })
        // The converter still names the refusal itself.
        expect(alertError).toHaveBeenCalledExactlyOnceWith(language.errors.noData)
        vi.mocked(alertError).mockClear()

        dismissNativeFileOperationOutcome()
        let release!: () => void
        const running = runSharedNativeFileOperation('import', 'risu-save-import', () => new Promise<null>(resolve => {
            release = () => resolve(null)
        }))
        await expect(importModule()).resolves.toBeNull()
        expect(alertError).toHaveBeenCalledExactlyOnceWith(language.risuNest.backup.fileBusy)
        release()
        await running
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
