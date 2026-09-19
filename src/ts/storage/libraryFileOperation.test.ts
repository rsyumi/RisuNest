import { beforeEach, describe, expect, it, vi } from 'vitest'

describe('library file admission settlement', () => {
    beforeEach(() => vi.resetModules())

    it('notifies once after release and an older release never clears a newer reservation', async () => {
        const admission = await import('./libraryFileOperation')
        const listener = vi.fn(() => expect(admission.isLibraryFileOperationReserved()).toBe(false))
        const unsubscribe = admission.subscribeLibraryFileOperationReleased(listener)
        const first = admission.reserveLibraryFileOperation()
        expect(listener).not.toHaveBeenCalled()
        first()
        const second = admission.reserveLibraryFileOperation()
        first()
        expect(admission.isLibraryFileOperationReserved()).toBe(true)
        expect(listener).toHaveBeenCalledOnce()
        second()
        expect(listener).toHaveBeenCalledTimes(2)
        unsubscribe()
        admission.reserveLibraryFileOperation()()
        expect(listener).toHaveBeenCalledTimes(2)
    })

    it('does not turn a settled operation into failure when a scheduling listener fails', async () => {
        const admission = await import('./libraryFileOperation')
        admission.subscribeLibraryFileOperationReleased(() => { throw new Error('synthetic scheduler failure') })
        const next = vi.fn()
        admission.subscribeLibraryFileOperationReleased(next)
        expect(() => admission.reserveLibraryFileOperation()()).not.toThrow()
        expect(admission.isLibraryFileOperationReserved()).toBe(false)
        expect(next).toHaveBeenCalledOnce()
    })
})
