import { describe, expect, it, vi } from 'vitest'

import { importDesktopNativeModulePath } from './nativeModuleFileRoute'

describe('native RISUM desktop route', () => {
    it('passes a mixed-case RISUM path to native import without reading bytes', async () => {
        const nativeImport = vi.fn(async () => ({ kind: 'imported' as const, value: 'module-id' }))
        const readDesktopPath = vi.fn()
        const legacyImport = vi.fn()

        await expect(importDesktopNativeModulePath(
            'C:\\chosen\\module.RISUM',
            { nativeImport, readDesktopPath, legacyImport },
        )).resolves.toEqual({ kind: 'imported', mode: 'native', value: 'module-id' })

        expect(nativeImport).toHaveBeenCalledWith({
            source: { type: 'desktopPath', path: 'C:\\chosen\\module.RISUM' },
            displayName: 'module.RISUM',
        })
        expect(readDesktopPath).not.toHaveBeenCalled()
        expect(legacyImport).not.toHaveBeenCalled()
    })

    it('keeps non-RISUM formats on the existing byte-based path', async () => {
        const bytes = Uint8Array.of(1, 2, 3)
        const nativeImport = vi.fn()
        const readDesktopPath = vi.fn(async () => bytes)
        const legacyImport = vi.fn(async () => 'legacy-id')

        await expect(importDesktopNativeModulePath(
            'C:\\chosen\\module.json',
            { nativeImport, readDesktopPath, legacyImport },
        )).resolves.toEqual({ kind: 'imported', mode: 'legacy', value: 'legacy-id' })

        expect(nativeImport).not.toHaveBeenCalled()
        expect(legacyImport).toHaveBeenCalledWith({ name: 'module.json', data: bytes })
    })

    it('does not retry through JavaScript after native preparation or CAS fails', async () => {
        const nativeImport = vi.fn(async () => { throw new Error('revision conflict') })
        const readDesktopPath = vi.fn()
        const legacyImport = vi.fn()

        await expect(importDesktopNativeModulePath(
            'C:\\chosen\\module.risum',
            { nativeImport, readDesktopPath, legacyImport },
        )).rejects.toThrow('revision conflict')

        expect(readDesktopPath).not.toHaveBeenCalled()
        expect(legacyImport).not.toHaveBeenCalled()
    })
})
