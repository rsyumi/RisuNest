import { describe, expect, it, vi } from 'vitest'

import {
    exportLocalBackupFromPicker,
    restoreLocalBackupFromPicker,
    type LosslessBackupFileRouteDependencies,
} from './losslessBackupFileRoute'

function dependencies(
    platform: LosslessBackupFileRouteDependencies['platform'] extends () => infer T ? T : never,
) {
    const calls: string[] = []
    const runtime = {
        revision: 7,
        flushPendingData: vi.fn(async () => undefined),
        capturePersistentMutationToken: vi.fn(async () => ({
            revision: 7,
            mutationGeneration: 1,
        })),
        acquireDestructiveReplacementFence: vi.fn(),
    }
    const deps: LosslessBackupFileRouteDependencies = {
        platform: () => platform,
        runtime: () => runtime,
        chooseNativeImport: vi.fn(async () => ({
            type: 'desktopPath' as const,
            path: 'C:\\picked\\backup.risulossless',
        })),
        chooseDesktopExport: vi.fn(async () => 'C:\\picked\\new.risulossless'),
        runNativeRestore: vi.fn(async (_runtime, source, options) => {
            calls.push(`restore:${source.type}`)
            await options.afterRefresh?.()
            return {
                revision: 8,
                sourceBytes: 4096,
                sourceSha256: 'a'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            }
        }),
        runNativeExport: vi.fn(async (_runtime, destination) => {
            calls.push(`export:${destination.type}`)
            return {
                revision: 7,
                sourceBytes: 2048,
                sourceSha256: 'b'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            }
        }),
        reloadPluginsAfterRestore: vi.fn(async () => {
            calls.push('plugins')
        }),
        saveLegacyBackup: vi.fn(async () => {
            calls.push('legacy-save')
        }),
        loadLegacyBackup: vi.fn(() => {
            calls.push('legacy-load')
        }),
    }
    return { calls, deps, runtime }
}

describe('lossless local backup file route', () => {
    it('uses the native lossless job and chosen path on desktop export', async () => {
        const { calls, deps, runtime } = dependencies('native-desktop')

        const result = await exportLocalBackupFromPicker({}, deps)

        expect(result).toMatchObject({ mode: 'native', bytes: 2048 })
        expect(deps.chooseDesktopExport).toHaveBeenCalledOnce()
        expect(deps.runNativeExport).toHaveBeenCalledWith(
            runtime,
            { type: 'desktopPath', path: 'C:\\picked\\new.risulossless' },
            expect.any(Object),
        )
        expect(calls).toEqual(['export:desktopPath'])
        expect(deps.saveLegacyBackup).not.toHaveBeenCalled()
    })

    it('publishes Android export through the existing SAF destination', async () => {
        const { calls, deps, runtime } = dependencies('native-android')

        const result = await exportLocalBackupFromPicker({}, deps)

        expect(result).toMatchObject({ mode: 'native', bytes: 2048 })
        expect(deps.chooseDesktopExport).not.toHaveBeenCalled()
        expect(deps.runNativeExport).toHaveBeenCalledWith(
            runtime,
            expect.objectContaining({
                type: 'androidSaf',
                suggestedName: expect.stringMatching(/\.risulossless$/),
            }),
            expect.any(Object),
        )
        expect(calls).toEqual(['export:androidSaf'])
    })

    it('keeps the legacy local backup writer on Web', async () => {
        const { calls, deps } = dependencies('web')

        const result = await exportLocalBackupFromPicker({}, deps)

        expect(result).toEqual({ mode: 'legacy', warningCodes: [] })
        expect(calls).toEqual(['legacy-save'])
        expect(deps.chooseDesktopExport).not.toHaveBeenCalled()
        expect(deps.runNativeExport).not.toHaveBeenCalled()
    })

    it('restores a desktop selection through the native replacement fence', async () => {
        const { calls, deps, runtime } = dependencies('native-desktop')

        const result = await restoreLocalBackupFromPicker({}, deps)

        expect(result).toMatchObject({ mode: 'native', bytes: 4096 })
        expect(deps.runNativeRestore).toHaveBeenCalledWith(
            runtime,
            { type: 'desktopPath', path: 'C:\\picked\\backup.risulossless' },
            expect.objectContaining({ afterRefresh: deps.reloadPluginsAfterRestore }),
        )
        expect(calls).toEqual(['restore:desktopPath', 'plugins'])
        expect(deps.loadLegacyBackup).not.toHaveBeenCalled()
    })

    it('restores Android from an owned spool token', async () => {
        const { calls, deps } = dependencies('native-android')
        vi.mocked(deps.chooseNativeImport).mockResolvedValueOnce({
            type: 'androidSpool',
            token: '55555555-5555-4555-8555-555555555555',
        })

        const result = await restoreLocalBackupFromPicker({}, deps)

        expect(result).toMatchObject({ mode: 'native', bytes: 4096 })
        expect(calls).toEqual(['restore:androidSpool', 'plugins'])
        expect(deps.runNativeRestore).toHaveBeenCalledWith(
            expect.any(Object),
            {
                type: 'androidSpool',
                token: '55555555-5555-4555-8555-555555555555',
            },
            expect.any(Object),
        )
    })

    it('treats closing the native import picker as cancellation without side effects', async () => {
        const { deps } = dependencies('native-android')
        vi.mocked(deps.chooseNativeImport).mockResolvedValueOnce(null)

        await expect(restoreLocalBackupFromPicker({}, deps)).resolves.toBeNull()
        expect(deps.runNativeRestore).not.toHaveBeenCalled()
    })

    it('keeps the legacy local backup reader on Web', async () => {
        const { calls, deps } = dependencies('web')

        const result = await restoreLocalBackupFromPicker({}, deps)

        expect(result).toEqual({ mode: 'legacy', warningCodes: [] })
        expect(calls).toEqual(['legacy-load'])
        expect(deps.chooseNativeImport).not.toHaveBeenCalled()
        expect(deps.runNativeRestore).not.toHaveBeenCalled()
    })
})
