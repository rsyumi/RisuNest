import { describe, expect, it, vi } from 'vitest'

import { importDesktopNativeModulePath } from './nativeModuleFileRoute'

describe('native RISUM desktop route', () => {
    it('passes a mixed-case RISUM path to native import without reading bytes', async () => {
        const nativeImport = vi.fn(async () => ({ kind: 'imported' as const, value: 'module-id' }))
        const controller = new AbortController()

        await expect(importDesktopNativeModulePath(
            'C:\\chosen\\module.RISUM',
            nativeImport,
            { signal: controller.signal },
        )).resolves.toEqual({ kind: 'imported', value: 'module-id' })

        expect(nativeImport).toHaveBeenCalledWith({
            source: { type: 'desktopPath', path: 'C:\\chosen\\module.RISUM' },
            displayName: 'module.RISUM',
        }, { signal: controller.signal })
    })

    it('does not retry through JavaScript after native preparation or CAS fails', async () => {
        const nativeImport = vi.fn(async () => { throw new Error('revision conflict') })

        await expect(importDesktopNativeModulePath(
            'C:\\chosen\\module.risum',
            nativeImport,
        )).rejects.toThrow('revision conflict')
    })
})
