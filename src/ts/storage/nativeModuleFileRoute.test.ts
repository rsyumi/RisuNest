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

    it('reports native preparation or commit failures without rejecting the UI handler', async () => {
        const error = new Error('revision conflict')
        const nativeImport = vi.fn(async () => { throw error })
        const reportError = vi.fn()

        await expect(importDesktopNativeModulePath(
            'C:\\chosen\\module.risum',
            nativeImport,
            {},
            reportError,
        )).resolves.toEqual({ kind: 'failed' })

        expect(reportError).toHaveBeenCalledWith(error)
        expect(nativeImport).toHaveBeenCalledOnce()
    })
})
