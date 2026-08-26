import { describe, expect, it, vi } from 'vitest'

import {
    NativeFileJobError,
} from './nativeFileJobs'
import {
    importRisuSaveFromPicker,
    exportRisuSaveFromPicker,
    type RisuSaveFileRouteDependencies,
} from './risuSaveFileRoute'

function dependencies(platform: 'native-desktop' | 'web'): RisuSaveFileRouteDependencies {
    return {
        platform: () => platform,
        runtime: () => ({
            revision: 4,
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: 4,
                mutationGeneration: 1,
            })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(async () => undefined),
                release: vi.fn(),
            })),
            replacePersistentDatabase: vi.fn(async () => undefined),
        }),
        chooseNativeImport: vi.fn(async () => 'C:\\chosen\\source.risudat'),
        chooseNativeExport: vi.fn(async () => 'C:\\chosen\\destination.risudat'),
        chooseWebImport: vi.fn(async () => [{
            name: 'source.risudat',
            arrayBuffer: async () => Uint8Array.from([1, 2, 3]).buffer,
        }]),
        runNativeImport: vi.fn(async (_runtime, _source, options) => {
            try {
                await options.afterRefresh?.()
            }
            catch (error) {
                const committed = new Error('committed refresh failed') as Error & {
                    name: string
                    committedRevision: number
                    recoveryRequired: boolean
                }
                committed.name = 'NativeFileJobActivationCommittedError'
                committed.committedRevision = 5
                committed.recoveryRequired = true
                throw committed
            }
            return {
                revision: 5,
                sourceBytes: 4096,
                sourceSha256: 'a'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            }
        }),
        runNativeExport: vi.fn(async () => ({
            revision: 4,
            sourceBytes: 8192,
            sourceSha256: 'b'.repeat(64),
            characterCount: 2,
            presetCount: 1,
            warningCodes: [],
        })),
        decodeRisuSave: vi.fn(async () => ({ username: 'Web import', characters: [] })),
        collectWebExport: vi.fn(async () => Uint8Array.from([4, 5, 6])),
        downloadWebExport: vi.fn(async () => undefined),
        reloadPlugins: vi.fn(async () => undefined),
        reloadPluginsAfterNativeRestore: vi.fn(async () => undefined),
    }
}

describe('RisuSave picker route', () => {
    it('passes only the selected desktop path to native import and refreshes through the job facade', async () => {
        const deps = dependencies('native-desktop')

        const result = await importRisuSaveFromPicker({}, deps)

        expect(result?.mode).toBe('native')
        expect(deps.runNativeImport).toHaveBeenCalledWith(
            expect.objectContaining({ revision: 4 }),
            { type: 'desktopPath', path: 'C:\\chosen\\source.risudat' },
            expect.objectContaining({
                afterRefresh: deps.reloadPluginsAfterNativeRestore,
                onStatus: undefined,
                signal: undefined,
            }),
        )
        expect(deps.decodeRisuSave).not.toHaveBeenCalled()
        expect(deps.chooseWebImport).not.toHaveBeenCalled()
        expect(deps.reloadPluginsAfterNativeRestore).toHaveBeenCalledOnce()
        expect(deps.reloadPlugins).not.toHaveBeenCalled()
    })

    it('publishes desktop export through the native job with omit-account unchanged', async () => {
        const deps = dependencies('native-desktop')

        const result = await exportRisuSaveFromPicker({ omitAccount: true }, deps)

        expect(result?.mode).toBe('native')
        expect(deps.runNativeExport).toHaveBeenCalledWith(
            expect.objectContaining({ revision: 4 }),
            'C:\\chosen\\destination.risudat',
            expect.objectContaining({ omitAccount: true }),
        )
        expect(deps.collectWebExport).not.toHaveBeenCalled()
        expect(deps.downloadWebExport).not.toHaveBeenCalled()
    })

    it('keeps the JavaScript codec only as the browser Web compatibility fallback', async () => {
        const deps = dependencies('web')
        const runtime = deps.runtime()
        deps.runtime = () => runtime

        const imported = await importRisuSaveFromPicker({}, deps)
        const exported = await exportRisuSaveFromPicker({ omitAccount: false }, deps)

        expect(imported?.mode).toBe('web')
        expect(exported?.mode).toBe('web')
        expect(deps.decodeRisuSave).toHaveBeenCalledWith(Uint8Array.from([1, 2, 3]))
        expect(runtime.replacePersistentDatabase).toHaveBeenCalledWith(
            { username: 'Web import', characters: [] },
            'risu-save-file-import',
            { authoritative: true },
        )
        expect(deps.collectWebExport).toHaveBeenCalledWith(false)
        expect(deps.downloadWebExport).toHaveBeenCalledWith(
            expect.stringMatching(/^risunest-.*\.risudat$/),
            Uint8Array.from([4, 5, 6]),
        )
        expect(deps.runNativeImport).not.toHaveBeenCalled()
        expect(deps.runNativeExport).not.toHaveBeenCalled()
    })

    it('falls back to the JavaScript importer when the desktop native capability is unavailable', async () => {
        const deps = dependencies('native-desktop')
        const runtime = deps.runtime()
        deps.runtime = () => runtime
        vi.mocked(deps.runNativeImport).mockRejectedValueOnce(
            new NativeFileJobError('capability-unavailable', 'native jobs unavailable'),
        )

        const result = await importRisuSaveFromPicker({}, deps)

        expect(result?.mode).toBe('web')
        expect(deps.chooseWebImport).toHaveBeenCalledOnce()
        expect(runtime.replacePersistentDatabase).toHaveBeenCalledOnce()
    })

    it('uses a separate Web picker with the JavaScript codec for unsupported formats', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.runNativeImport).mockRejectedValueOnce(
            new NativeFileJobError('unsupported-format', 'not a block save'),
        )

        const result = await importRisuSaveFromPicker({}, deps)

        expect(result?.mode).toBe('web')
        expect(deps.chooseWebImport).toHaveBeenCalledOnce()
        expect(deps.decodeRisuSave).toHaveBeenCalledWith(Uint8Array.from([1, 2, 3]))
    })

    it('does not fall back for invalid block input', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.runNativeImport).mockRejectedValueOnce(
            new NativeFileJobError('corrupt-input', 'invalid gzip stream'),
        )

        await expect(importRisuSaveFromPicker({}, deps)).rejects.toMatchObject({
            code: 'corrupt-input',
        })
        expect(deps.chooseWebImport).not.toHaveBeenCalled()
        expect(deps.decodeRisuSave).not.toHaveBeenCalled()
    })

    it('reports plugin refresh failure as post-commit recovery instead of restore failure', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.reloadPluginsAfterNativeRestore).mockRejectedValueOnce(
            new Error('plugin refresh failed'),
        )

        await expect(importRisuSaveFromPicker({}, deps)).rejects.toEqual(
            expect.objectContaining({
                name: 'NativeFileJobActivationCommittedError',
                committedRevision: 5,
                recoveryRequired: true,
            }),
        )
    })

    it('falls back to the JavaScript exporter when the desktop native capability is unavailable', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.runNativeExport).mockRejectedValueOnce(
            new NativeFileJobError('capability-unavailable', 'native jobs unavailable'),
        )

        const result = await exportRisuSaveFromPicker({ omitAccount: true }, deps)

        expect(result?.mode).toBe('web')
        expect(deps.collectWebExport).toHaveBeenCalledWith(true)
        expect(deps.downloadWebExport).toHaveBeenCalledOnce()
    })

    it('does nothing when either picker is cancelled', async () => {
        const deps = dependencies('native-desktop')
        vi.mocked(deps.chooseNativeImport).mockResolvedValueOnce(null)
        vi.mocked(deps.chooseNativeExport).mockResolvedValueOnce(null)

        await expect(importRisuSaveFromPicker({}, deps)).resolves.toBeNull()
        await expect(exportRisuSaveFromPicker({}, deps)).resolves.toBeNull()
        expect(deps.runNativeImport).not.toHaveBeenCalled()
        expect(deps.runNativeExport).not.toHaveBeenCalled()
    })
})
