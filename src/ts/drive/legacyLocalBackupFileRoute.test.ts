import { describe, expect, it, vi } from 'vitest'

import {
    exportLegacyLocalBackupFromPicker,
    importLegacyLocalBackupFromPicker,
    isNativeLegacyBackupFallback,
    type LegacyLocalBackupFileRouteDependencies,
} from './legacyLocalBackupFileRoute'

function dependencies(): LegacyLocalBackupFileRouteDependencies {
    return {
        runtime: () => ({
            revision: 7,
            flushPendingData: vi.fn(async () => undefined),
            capturePersistentMutationToken: vi.fn(async () => ({
                revision: 7,
                mutationGeneration: 3,
            })),
            acquireDestructiveReplacementFence: vi.fn(async () => ({
                refreshCommittedWorkingSet: vi.fn(async () => undefined),
                release: vi.fn(),
            })),
        }),
        chooseImport: vi.fn(async () => ({
            type: 'desktopPath' as const,
            path: 'C:\\chosen\\backup.bin',
        })),
        chooseExport: vi.fn(async () => ({
            type: 'desktopPath' as const,
            path: 'C:\\chosen\\backup.bin',
        })),
        runImport: vi.fn(async (_runtime, _source, options) => {
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
        runExport: vi.fn(async () => ({
            revision: 7,
            sourceBytes: 8192,
            sourceSha256: 'b'.repeat(64),
            characterCount: 2,
            presetCount: 1,
            warningCodes: [],
        })),
        reloadPluginsAfterRestore: vi.fn(async () => undefined),
    }
}

describe('legacy local backup native file route', () => {
    it('imports from a native path without reading archive bytes in TypeScript', async () => {
        const deps = dependencies()

        const result = await importLegacyLocalBackupFromPicker({}, deps)

        expect(result?.sourceBytes).toBe(4096)
        expect(deps.runImport).toHaveBeenCalledWith(
            expect.objectContaining({}),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.bin' },
            expect.objectContaining({ afterRefresh: deps.reloadPluginsAfterRestore }),
        )
        expect(deps.reloadPluginsAfterRestore).toHaveBeenCalledOnce()
        expect(JSON.stringify(vi.mocked(deps.runImport).mock.calls)).not.toContain('Uint8Array')
    })

    it('exports to a native destination without sending entry descriptors or bytes', async () => {
        const deps = dependencies()

        const result = await exportLegacyLocalBackupFromPicker({}, deps)

        expect(result?.sourceBytes).toBe(8192)
        expect(deps.runExport).toHaveBeenCalledWith(
            expect.objectContaining({ revision: 7 }),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.bin' },
            expect.objectContaining({ signal: undefined }),
        )
        expect(JSON.stringify(vi.mocked(deps.runExport).mock.calls)).not.toContain('Uint8Array')
    })

    it('hands the source callback to the picker without forwarding it to the native job', async () => {
        const deps = dependencies()
        const onSource = vi.fn()
        vi.mocked(deps.chooseImport).mockImplementationOnce(async (options) => {
            options.onSource?.({ name: 'backup.bin', bytes: 4096 })
            return { type: 'desktopPath' as const, path: 'C:\\chosen\\backup.bin' }
        })

        await importLegacyLocalBackupFromPicker({ onSource }, deps)

        expect(onSource).toHaveBeenCalledExactlyOnceWith({ name: 'backup.bin', bytes: 4096 })
        expect(vi.mocked(deps.runImport).mock.calls[0][2]).not.toHaveProperty('onSource')
    })

    it('treats only missing capability and unsupported formats as WebView fallbacks', () => {
        expect(isNativeLegacyBackupFallback({ code: 'capability-unavailable' })).toBe(true)
        expect(isNativeLegacyBackupFallback({ code: 'unsupported-format' })).toBe(true)
        expect(isNativeLegacyBackupFallback({ code: 'corrupt-input' })).toBe(false)
        expect(isNativeLegacyBackupFallback(new Error('plain'))).toBe(false)
        expect(isNativeLegacyBackupFallback(null)).toBe(false)
    })

    it('does not start a job when the system picker is cancelled', async () => {
        const deps = dependencies()
        vi.mocked(deps.chooseImport).mockResolvedValueOnce(null)
        vi.mocked(deps.chooseExport).mockResolvedValueOnce(null)

        await expect(importLegacyLocalBackupFromPicker({}, deps)).resolves.toBeNull()
        await expect(exportLegacyLocalBackupFromPicker({}, deps)).resolves.toBeNull()
        expect(deps.runImport).not.toHaveBeenCalled()
        expect(deps.runExport).not.toHaveBeenCalled()
    })
})
