import { expect, it, vi } from 'vitest'
import { reportPresetOperation } from './presetOperation'

it('reports a rejected preset operation and returns failure', async () => {
    const reportFailure = vi.fn()

    await expect(reportPresetOperation(
        () => Promise.reject(new Error('revision changed')),
        reportFailure,
    )).resolves.toBe(false)
    expect(reportFailure).toHaveBeenCalledOnce()
})

it('returns success without reporting when the preset operation completes', async () => {
    const reportFailure = vi.fn()

    await expect(reportPresetOperation(
        () => Promise.resolve(),
        reportFailure,
    )).resolves.toBe(true)
    expect(reportFailure).not.toHaveBeenCalled()
})
